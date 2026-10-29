//! Engine-free definition/data types of the extension system.
//!
//! These are the shapes the host hands across the Rust/JS boundary and that the
//! plugin framework, the session loop and the TUI read directly (`ToolDef`,
//! `CommandDef`, `Notification`, `ToolResult`, ...). They carry no
//! QuickJS/swc/tokio dependency, so they compile in every feature
//! configuration: a minimal SDK build (no `extensions` feature) keeps the whole
//! plugin/hook data surface, only the JS engine is trimmed.
//!
//! The engine side (`runtime`, `handle`, `loader`, `transpile`, `api`) lives in
//! the sibling modules behind `cfg(feature = "extensions")`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::path::PathBuf;
use std::time::Duration;

/// Default timeout applied to async JS calls (tool execution, dispatch).
///
/// Lives next to the data types (rather than in `runtime`) so hosts that only
/// carry the plugin/hook surface can state it without the engine; the engine
/// enforces it.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Long deadline for reserved-prefix hook calls (`__pir_stream:` /
/// `__pir_route:`): these keys route to a JS `streamSimple` / virtual-model
/// `route(request)` callback, and a real model backend can legitimately keep
/// one `streamSimple` round trip running for many minutes — far beyond
/// [`DEFAULT_CALL_TIMEOUT`]. Ordinary tool `prepareLoadout` hooks keep the
/// short [`DEFAULT_CALL_TIMEOUT`]; a reserved key can never collide with a
/// real tool name (tool names are validated identifiers).
pub const HOOK_CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// One item of an incremental JS hook event stream (upstream's live
/// `AssistantMessageEventStream`, pulled event-by-event across the QuickJS
/// boundary by [`ExtensionRuntime::call_hook_stream`]).
#[derive(Debug, Clone)]
pub enum HookStreamItem {
    /// One assistant event in wire-JSON form (validated by the consumer).
    Event(JsonValue),
    /// The JS side finished cleanly.
    End,
    /// Fail-closed terminal: the JS callback threw, a deadline passed, or a
    /// reply shape was invalid. The consumer terminates its stream with a
    /// typed error event.
    Failed(String),
}

/// Bridge for one in-flight streamSimple request's options hooks (upstream
/// `SimpleStreamOptions.onPayload` / `onResponse` /
/// `onProviderStreamEvent`, which `buildBaseOptions` forwards into the API
/// adapter's `StreamOptions`): the JS `streamSimple` callback receives
/// callable wrappers in its `options` object (the prelude's
/// `__pir_startProviderStream` installs them when the request carries a
/// `hookSession`), and each JS call lands on the matching Rust observer via
/// the `__pir_host.streamHookEvent` host fn. Fire-and-forget: hook errors or
/// panics never interrupt the stream, and `on_payload` mutations cannot
/// round-trip to JS (the JS callback owns the payload — observation-only
/// passthrough).
#[derive(Clone, Default)]
pub struct StreamHookBridge {
    /// Upstream `onPayload`: invoked with the serialized request body before
    /// the callback's backend call.
    pub on_payload: Option<crate::ai::types::StreamHookMut>,
    /// Upstream `onResponse`: invoked with `(status, headers)` once the
    /// backend response head is known.
    pub on_response: Option<StreamResponseHook>,
    /// Upstream `onProviderStreamEvent`: invoked with each raw provider event
    /// before normalization.
    pub on_provider_stream_event: Option<crate::ai::types::StreamHook>,
}

/// Response observer type, matching `crate::ai::types::stream::ResponseHook`
/// (that module is private, so the bridge carries a structural alias).
pub type StreamResponseHook =
    std::sync::Arc<dyn Fn(u16, &std::collections::BTreeMap<String, String>) + Send + Sync>;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Upstream `SourceInfo` (`core/source-info.ts:6-12`): where a resource came
/// from. `path` is the only field every host needs; `source`/`scope`/`origin`
/// and `baseDir` carry the provenance RPC `get_commands` reports
/// (`rpc-types.ts:81-90`).
///
/// Every added field is serde-defaulted and omitted when empty, so an older
/// producer (the JS prelude exports `sourceInfo: {path}` only) or an older
/// session/extension payload still deserializes, and a resource whose
/// provenance pir does not know reports just its path instead of a made-up
/// scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    #[serde(default)]
    pub path: String,
    /// How the resource was found: `"local"`, `"package"`, `"builtin"`,
    /// `"cli"`, … (upstream `PathMetadata.source`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// `"user"`, `"project"` or `"temporary"` (upstream `SourceScope`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
    /// `"package"` or `"top-level"` (upstream `SourceOrigin`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    /// The directory the resource was resolved from (upstream `baseDir`).
    #[serde(rename = "baseDir", default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

impl SourceInfo {
    /// A path-only source (the shape an extension that declares no provenance
    /// produces).
    pub fn from_path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    /// Build the upstream synthetic shape (source-info.ts
    /// `createSyntheticSourceInfo`): `scope` defaults to `"temporary"` and
    /// `origin` to `"top-level"`.
    pub fn synthetic(
        path: impl Into<String>,
        source: impl Into<String>,
        scope: Option<&str>,
        origin: Option<&str>,
        base_dir: Option<String>,
    ) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
            scope: scope.unwrap_or("temporary").to_owned(),
            origin: origin.unwrap_or("top-level").to_owned(),
            base_dir,
        }
    }

    /// Provenance inferred from a resource's location, mirroring upstream
    /// `getDefaultSourceInfoForPath` (resource-loader.ts:1013-1062):
    /// `<agentDir>/{skills,prompts,themes,extensions}` is the user scope,
    /// `<cwd>/.pi/{...}` the project scope, and everything else a temporary
    /// top-level resource whose `baseDir` is its containing directory.
    /// Synthetic paths (`builtin:<name>`, `<inline:name>`) report their
    /// prefix as `source` and the temporary scope, exactly like upstream
    /// `getSyntheticPathSource`.
    pub fn for_resource_path(
        path: impl AsRef<std::path::Path>,
        cwd: &std::path::Path,
        agent_dir: &std::path::Path,
    ) -> Self {
        let path = path.as_ref();
        let path_text = path.to_string_lossy().into_owned();
        if let Some(source) = synthetic_path_source(&path_text) {
            return Self::synthetic(
                path_text,
                source,
                Some("temporary"),
                Some("top-level"),
                None,
            );
        }
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let roots = [
            ("user", agent_dir.join("skills")),
            ("user", agent_dir.join("prompts")),
            ("user", agent_dir.join("themes")),
            ("user", agent_dir.join("extensions")),
            ("project", cwd.join(".pi").join("skills")),
            ("project", cwd.join(".pi").join("prompts")),
            ("project", cwd.join(".pi").join("themes")),
            ("project", cwd.join(".pi").join("extensions")),
        ];
        for (scope, root) in roots {
            let root = std::path::absolute(&root).unwrap_or(root);
            if absolute.starts_with(&root) {
                return Self::synthetic(
                    path_text,
                    "local",
                    Some(scope),
                    Some("top-level"),
                    Some(root.to_string_lossy().into_owned()),
                );
            }
        }
        let base_dir = absolute
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned());
        Self::synthetic(
            path_text,
            "local",
            Some("temporary"),
            Some("top-level"),
            base_dir,
        )
    }

    /// Fill the EMPTY/absent provenance fields from the path's location
    /// ([`Self::for_resource_path`]); a producer that sent richer metadata
    /// keeps every value it sent. A source with no path at all is returned
    /// unchanged (nothing can be inferred and nothing is invented).
    pub fn filled_from_path(mut self, cwd: &std::path::Path, agent_dir: &std::path::Path) -> Self {
        if self.path.is_empty() {
            return self;
        }
        let inferred = Self::for_resource_path(&self.path, cwd, agent_dir);
        if self.source.is_empty() {
            self.source = inferred.source;
        }
        if self.scope.is_empty() {
            self.scope = inferred.scope;
        }
        if self.origin.is_empty() {
            self.origin = inferred.origin;
        }
        if self.base_dir.is_none() {
            self.base_dir = inferred.base_dir;
        }
        self
    }
}

/// Source of a synthetic path (upstream `getSyntheticPathSource`): `builtin`
/// for `builtin:<name>`, the angle-bracket prefix for `<inline:name>`, `None`
/// for real file paths.
pub fn synthetic_path_source(path: &str) -> Option<String> {
    if path.starts_with("builtin:") {
        return Some("builtin".to_owned());
    }
    if path.starts_with('<') && path.ends_with('>') {
        let inner = &path[1..path.len() - 1];
        let source = inner.split(':').next().unwrap_or_default();
        return Some(if source.is_empty() {
            "temporary".to_owned()
        } else {
            source.to_owned()
        });
    }
    None
}

/// Outcome of the `user_bash` extension event for a `!`-prefixed command
/// (upstream `UserBashEventResult`).
#[derive(Debug, Clone)]
pub enum UserBashOutcome {
    /// No handler decided otherwise: run the command locally.
    Local,
    /// The extension executed the command; use its result verbatim.
    Handled { output: String, is_error: bool },
    /// The extension supplied custom operations for execution. pir has no
    /// model-side bash operations channel, so the caller reports them.
    Operations { operations: JsonValue },
    /// A handler threw or returned an invalid shape: the command is NOT run
    /// locally (upstream interactive-mode refuses the fallback).
    Aborted,
}

/// Metadata for an extension-registered LLM tool (no `execute` —that stays
/// inside QuickJS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "promptSnippet", default)]
    pub prompt_snippet: Option<String>,
    #[serde(rename = "promptGuidelines", default)]
    pub prompt_guidelines: Vec<String>,
    #[serde(default)]
    pub parameters: JsonValue,
    /// JSON Schema of `structuredContent` in successful results (upstream
    /// `AgentTool.outputSchema`); `Null` when the tool declares none.
    #[serde(rename = "outputSchema", default)]
    pub output_schema: JsonValue,
    #[serde(rename = "sourceInfo", default)]
    pub source_info: SourceInfo,
    #[serde(rename = "executionMode", default)]
    pub execution_mode: String,
    /// How the model reaches the tool (upstream `ToolDefinition.exposure`):
    /// `"direct" | "model-only" | "codemode" | "deferred" | "hidden"`.
    #[serde(default)]
    pub exposure: String,
    /// Group the tool belongs to, for example its MCP server (upstream
    /// `ToolDefinition.namespace`).
    #[serde(default)]
    pub namespace: Option<ToolNamespaceDef>,
    /// Hints about what the tool does (upstream `ToolDefinition.annotations`).
    #[serde(default)]
    pub annotations: Option<ToolAnnotationsDef>,
    /// Whether registering the tool activates it (upstream
    /// `ToolDefinition.defaultActive`).
    #[serde(rename = "defaultActive", default = "true_default")]
    pub default_active: bool,
    /// Whether the tool declares a `prepareLoadout` hook (the function stays
    /// inside QuickJS; the session invokes it through the runtime).
    #[serde(rename = "prepareLoadout", default)]
    pub prepare_loadout: bool,
    /// Snapshot of the tool's `reachesTools` hook (upstream
    /// `ToolDefinition.reachesTools`; the live function stays inside QuickJS).
    /// The registry export probes the hook once with the empty input — the
    /// same probe `ensureDiscoveryActive` makes on Rust tools — because the
    /// export is a synchronous channel and a function cannot be serialized;
    /// the live per-call value is pulled through the runtime at the tool-call
    /// wait seam. Absent for tools that declare no reach.
    #[serde(
        rename = "reachesTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reaches_tools: Option<ToolReachDef>,
}

/// Wire shape of a JS tool's `reachesTools` snapshot (upstream
/// `ToolDefinition.reachesTools`): `"all"`, the hook's word list, or `true`
/// when the hook declared a reach the empty-input probe could not serialize.
/// A transparent newtype over the raw JSON value, so an unexpected shape is a
/// declared reach without words instead of a registry-parse failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolReachDef(pub JsonValue);

impl ToolReachDef {
    /// Whether the snapshot is the `"all"` value.
    pub(crate) fn is_all(&self) -> bool {
        matches!(&self.0, JsonValue::String(text) if text == "all")
    }

    /// The word list of the snapshot; an empty list for any shape that is not
    /// `"all"` and not an array of strings (a non-string entry is not a name a
    /// namespace could match and is dropped).
    pub(crate) fn words(&self) -> Vec<String> {
        match &self.0 {
            JsonValue::Array(items) => items
                .iter()
                .filter_map(|item| match item {
                    JsonValue::String(text) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Default for [`ToolDef::default_active`] (serde needs a path).
fn true_default() -> bool {
    true
}

/// Wire shape of a JS `registerTool` namespace (upstream `ToolNamespace`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolNamespaceDef {
    #[serde(default)]
    pub name: String,
    /// Other names that refer to the namespace where tools look it up by name
    /// (upstream `ToolNamespace.aliases`; codemode's `describeNamespace()`
    /// and the `namespace` option of `searchTools()`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Wire shape of a JS `registerTool` annotations object (upstream
/// `ToolAnnotations`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ToolAnnotationsDef {
    #[serde(rename = "readOnlyHint", default)]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", default)]
    pub destructive_hint: Option<bool>,
    #[serde(rename = "idempotentHint", default)]
    pub idempotent_hint: Option<bool>,
    #[serde(rename = "openWorldHint", default)]
    pub open_world_hint: Option<bool>,
}

/// A content item inside a [`ToolResult`].
///
/// Text items carry [`Self::text`]; image items (`kind == "image"`, upstream
/// `{type: "image", data, mimeType}`) carry the base64 payload and its mime
/// type. The latter used to be dropped at this boundary: a JS or native tool
/// returning an image reached Rust as a text-only item with no data, so the
/// model never saw it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentItem {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    /// Base64 payload of an image item (absent for text items).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// MIME type of an image item (absent for text items).
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl ContentItem {
    /// A text block (the shape every pre-image caller builds).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text".to_owned(),
            text: text.into(),
            data: None,
            mime_type: None,
        }
    }

    /// An image block (upstream `{type: "image", data, mimeType}`).
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            kind: "image".to_owned(),
            text: String::new(),
            data: Some(data.into()),
            mime_type: Some(mime_type.into()),
        }
    }
}

/// Result of executing an extension tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default)]
    pub content: Vec<ContentItem>,
    #[serde(default)]
    pub details: JsonValue,
    /// Machine-readable output validated against the tool's `outputSchema`
    /// (upstream `AgentToolResult.structuredContent`); `Null` when absent.
    #[serde(rename = "structuredContent", default)]
    pub structured_content: JsonValue,
    #[serde(default)]
    pub terminate: bool,
}

/// Metadata for an extension-registered slash command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Command source tag (`"extension"`), mirroring upstream
    /// `getCommands()` so `source`-filtering extensions work.
    #[serde(default)]
    pub source: String,
    #[serde(rename = "sourceInfo", default)]
    pub source_info: SourceInfo,
    /// Whether the command registered a JS `getArgumentCompletions` hook
    /// (upstream `RegisteredCommand.getArgumentCompletions`); the TUI pays
    /// the async completion round-trip only when this is set.
    #[serde(rename = "argumentCompletions", default)]
    pub argument_completions: bool,
}

/// A CLI flag registered by an extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlagDef {
    pub name: String,
    #[serde(rename = "type")]
    pub flag_type: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<FlagValue>,
    #[serde(rename = "extId", default)]
    pub ext_id: u64,
}

/// Flag values are booleans or strings (spec §1.2 `registerFlag`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue {
    Bool(bool),
    Str(String),
}

/// Metadata for an extension-registered keyboard shortcut (upstream
/// `registerShortcut`). The TUI host enumerates the registry with
/// [`crate::extensions::ExtensionHandle::shortcuts`] and dispatches key
/// presses with [`crate::extensions::ExtensionHandle::invoke_shortcut`]; the
/// handler result (and any `ctx.ui.*` side effects) are marshalled back
/// through the actor mailbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutDef {
    /// Key combo string exactly as registered (e.g. `"ctrl+shift+k"`).
    pub key: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "sourceInfo", default)]
    pub source_info: SourceInfo,
}

/// Host-pushed snapshot of live session state backing the `ctx` facade
/// (`ctx.model`, `ctx.sessionManager`, `ctx.getContextUsage`, ...). The
/// session host refreshes it via
/// [`crate::extensions::ExtensionHandle::update_session_context`] before each
/// prompt; the extension runtime only reads it.
#[derive(Debug, Clone, Default)]
pub struct SessionContextSnapshot {
    /// Serialized current `Model` object (or `Null`).
    pub model: JsonValue,
    /// Serialized session entries (`Vec<Message>` JSON, `Null` when none).
    pub entries: JsonValue,
    /// Serialized session list (`[{id, path, cwd, modifiedMs}]`, `Null`).
    pub sessions: JsonValue,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
    /// Estimated context tokens (last usage), when known.
    pub context_tokens: Option<u64>,
    /// Current model context window, when known.
    pub context_window: Option<u64>,
    /// Whether steering/follow-up messages are queued.
    pub has_pending_messages: bool,
    /// Whether project-local trust is active.
    pub project_trusted: bool,
    /// Working directory (`ctx.sessionManager.getCwd()`).
    pub cwd: Option<String>,
    /// Serialized session header (`getHeader()`); `Null` when none.
    pub header: JsonValue,
    /// Serialized branch (leaf-path) entries (`getBranch()`); `Null`.
    pub branch: JsonValue,
    /// The full entry tree (`getTree()` / `getEntry()` / `getLeafEntry()`) as
    /// PRE-RENDERED JSON TEXT — the array `tree()` nests, written by
    /// [`crate::session_manager::TreeNode::forest_to_json_iterative`] — or
    /// empty when no session is attached.
    ///
    /// Text, not a `Value`: the tree nests one level per entry, and a deeply
    /// nested `Value` recurses per level in `serde_json`'s serializer AND in
    /// its drop glue, which overran a 2 MiB thread while pushing a
    /// 2000-entry session's snapshot. `getSessionManagerJson` splices this
    /// text into the reply, so the wire shape is unchanged
    /// (`"tree": <the same array>`).
    pub tree_json: String,
    /// Leaf entry id (`getLeafId()`).
    pub leaf_id: Option<String>,
    /// Serialized `{entryId: label}` map (`getLabel()`).
    pub labels: JsonValue,
    /// Serialized session projection (`buildSessionProjection()`):
    /// `{entries, messages}`; `Null`.
    pub projection: JsonValue,
}

/// Outcome of dispatching an event through all registered handlers.
#[derive(Debug, Clone, Default)]
pub struct EmitOutcome {
    /// Merged event result (event-specific shape, `Null` when no handler
    /// produced a result).
    pub result: JsonValue,
    /// Final payload after in-place handler mutations (only populated for
    /// `tool_call`).
    pub payload: JsonValue,
    /// Errors thrown by individual handlers (isolated, never fatal).
    pub errors: Vec<EventError>,
}

/// An error raised by a single event handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventError {
    #[serde(rename = "extId")]
    pub ext_id: u64,
    pub event: String,
    pub error: String,
}

/// Outcome of dispatching a slash command.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub ok: bool,
    pub error: Option<String>,
}

/// What an extension renderer callback produced (`__pir_callRenderer`):
/// styled transcript lines (message/entry/tool renderers) or transformed
/// markdown text (markdown transformer). Both `None` means "not handled" —
/// the caller keeps its default rendering.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RendererOutcome {
    pub lines: Option<Vec<String>>,
    pub text: Option<String>,
}

impl RendererOutcome {
    pub fn is_unhandled(&self) -> bool {
        self.lines.is_none() && self.text.is_none()
    }
}

/// The upstream `ToolDefinition.renderShell` choice
/// (`extensions/types.ts:595`, `"default" | "self"`).
///
/// `Default` means the host paints the renderer's lines inside the standard
/// colored tool shell (`ToolExecutionComponent`'s `contentBox`); `SelfShell`
/// means the tool renders its own framing and the lines are painted bare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRenderShell {
    Default,
    SelfShell,
}

impl ToolRenderShell {
    /// Parse the wire spelling (`"default"` / `"self"`). Unknown values stay
    /// `None` so a future upstream value degrades to the host's legacy
    /// rendering instead of guessing.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "self" => Some(Self::SelfShell),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::SelfShell => "self",
        }
    }
}

/// One renderer callback's output plus the metadata the scalar
/// [`RendererOutcome`] does not model.
///
/// Today it carries the tool `renderShell` choice a tool renderer call
/// returned (`__pir_callToolRenderer` / the `toolRenderers` resolver chain).
/// The type is separate from [`RendererOutcome`] so every existing renderer
/// implementation and test literal keeps compiling; callers that only need
/// lines/text use [`RendererCallOutcome::outcome`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RendererCallOutcome {
    pub outcome: RendererOutcome,
    /// `Some` only when the call is a tool renderer whose registration (or
    /// resolver chain) declared a `renderShell`; non-tool calls leave it
    /// `None`.
    pub shell: Option<ToolRenderShell>,
}

impl RendererCallOutcome {
    /// Wrap a plain outcome that carries no shell metadata.
    pub fn from_outcome(outcome: RendererOutcome) -> Self {
        Self {
            outcome,
            shell: None,
        }
    }

    /// `lines` is `None` and `text` is `None` — the caller keeps its default.
    pub fn is_unhandled(&self) -> bool {
        self.outcome.is_unhandled()
    }
}

/// One discovered extension entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedExtension {
    /// Extension display path (the file that was loaded).
    pub path: PathBuf,
    /// Synthetic QuickJS module name for this entry point.
    pub module_name: String,
}

/// An error entry for a path that failed to load.
#[derive(Debug, Clone)]
pub struct ExtensionLoadError {
    pub path: PathBuf,
    pub error: String,
}

/// Result of loading a set of extensions.
#[derive(Debug, Clone, Default)]
pub struct LoadResult {
    pub loaded: Vec<LoadedExtension>,
    pub errors: Vec<ExtensionLoadError>,
}

/// Records captured from extension side effects, drained by the host.
#[derive(Debug, Clone)]
pub enum Notification {
    Notify {
        message: String,
        level: String,
    },
    Status {
        key: String,
        text: Option<String>,
    },
    SendMessage {
        payload: JsonValue,
    },
    SendUserMessage {
        payload: JsonValue,
    },
    AppendEntry {
        payload: JsonValue,
    },
    Abort,
    SetSessionName(Option<String>),
    SetLabel {
        entry_id: String,
        label: Option<String>,
    },
    ToolUpdate {
        tool_call_id: String,
        /// Name of the tool that produced the update (parity payload).
        tool_name: Option<String>,
        /// The tool's input arguments (parity payload).
        args: Option<JsonValue>,
        /// Parent tool call id for nested calls (`<parent>/<n>` ids).
        parent_call_id: Option<String>,
        partial: Option<JsonValue>,
    },
    ActiveToolsOverride(Vec<String>),
    /// `ctx.ui.setTitle(text)` —TUI hosts apply it to the terminal title;
    /// headless hosts ignore it.
    SetTitle(String),
    /// An interactive `ctx.ui.select/confirm/input/editor` request awaiting a
    /// TUI reply (`ExtensionHandle::respond_dialog`). Only produced when the
    /// runtime was built with `has_ui`; headless hosts never see one.
    DialogRequest {
        /// Correlation id matching the reply.
        id: u64,
        /// `select` | `confirm` | `input` | `editor`.
        kind: String,
        title: Option<String>,
        /// Select options / input placeholder / editor prefill, as provided.
        options: JsonValue,
    },
    /// `ctx.ui.setWorkingMessage` —TUI hosts show the message in place of
    /// the default working indicator while streaming; an empty string is the
    /// reset-to-default signal (upstream `setWorkingMessage(undefined)`).
    WorkingMessage(String),
    /// `ctx.ui.setWorkingVisible` (upstream types.ts:171-172): show or hide
    /// the built-in working loader row. A distinct channel — encoding a hide
    /// as an empty working message would read as "streaming" to the TUI
    /// (its run state is derived from the working-message slot).
    WorkingVisible(bool),
    /// `ctx.ui.setEditorText(text)` —TUI hosts replace the live editor
    /// text; headless hosts ignore it.
    EditorSetText(String),
    /// `ctx.ui.pasteToEditor(text)` —TUI hosts paste into the live editor
    /// (newline normalization + cursor placement); headless hosts ignore it.
    EditorPaste(String),
    /// `ctx.ui.setTheme(name)` —TUI hosts swap the live theme (custom
    /// themes resolve from the registry); headless hosts ignore it.
    SetTheme(String),
    /// `ctx.ui.addAutocompleteProvider(fn)` —a JS completion suggester was
    /// registered; TUI hosts consult it (bounded, async) for argument and
    /// general completions.
    AutocompleteProviderRegistered,
    /// `ctx.ui.setWidget(id, content)` —a `Null` content removes the widget.
    /// TUI hosts render a single line per widget; headless hosts ignore it.
    Widget {
        id: String,
        content: JsonValue,
    },
    /// `ctx.ui.setFooter(factory)` / `setFooter(undefined)` (upstream
    /// types.ts:195-204): `true` registers the footer factory slot (the
    /// prelude renders `component.render(width)` through the
    /// `footerRenderers` renderer round-trip), `false` restores the built-in
    /// footer.
    Footer(bool),
    /// `ctx.ui.setHeader(factory)` / `setHeader(undefined)` (upstream
    /// types.ts:206-208): `true` registers the header slot (rendered through
    /// the `headerRenderers` round-trip), `false` restores the built-in
    /// startup header.
    Header(bool),
    /// `ctx.ui.setWorkingIndicator(options?)` (upstream types.ts:176-184):
    /// the custom frames/interval for the streaming indicator, `Null` to
    /// restore the default spinner. Shape (prelude-normalized):
    /// `{frames?: string[], intervalMs?: number}`; `frames: []` hides the
    /// indicator entirely.
    WorkingIndicator(JsonValue),
    /// `ctx.ui.setHiddenThinkingLabel(label?)` (upstream types.ts:185): the
    /// label rendered for hidden thinking blocks; `None` restores the
    /// default ("Thinking...").
    HiddenThinkingLabel(Option<String>),
    /// `pi.setModel(model)` —the session host resolves the model against the
    /// registry, applies it, and emits `model_select`.
    SetModel {
        provider: Option<String>,
        model_id: String,
    },
    /// `pi.setThinkingLevel(level)` —the session host applies it and emits
    /// `thinking_level_select`.
    SetThinkingLevel(String),
    /// `ctx.compact(options)` —the session host runs manual compaction after
    /// the in-flight run settles (upstream triggers it without awaiting).
    CompactRequest {
        custom_instructions: Option<String>,
    },
    /// `ctx.shutdown()` —graceful shutdown request; headless hosts log it,
    /// TUI hosts exit the main loop.
    ShutdownRequest,
}

impl Notification {
    /// Presentation-class notifications: consumed by the TUI drain (status
    /// line, widgets, working message, terminal title). The session host
    /// never steals these, and the TUI never steals actionable ones — the
    /// classes partition the queue so shared-handle drains compose.
    pub fn is_presentation(&self) -> bool {
        matches!(
            self,
            Notification::Notify { .. }
                | Notification::Status { .. }
                | Notification::Widget { .. }
                | Notification::Footer(_)
                | Notification::Header(_)
                | Notification::WorkingIndicator(_)
                | Notification::HiddenThinkingLabel(_)
                | Notification::WorkingMessage(_)
                | Notification::WorkingVisible(_)
                | Notification::SetTitle(_)
                | Notification::DialogRequest { .. }
                | Notification::EditorSetText(_)
                | Notification::EditorPaste(_)
                | Notification::SetTheme(_)
                | Notification::AutocompleteProviderRegistered
        )
    }

    /// Actionable notifications: consumed by the session host
    /// (steer/follow-up queues, custom entries, labels, model/thinking
    /// level, active-tools override, ...).
    pub fn is_actionable(&self) -> bool {
        !self.is_presentation()
    }
}

// Note: the always-`None` `HostBridge` seam was removed. All extension side
// effects flow through [`Notification`]s and are consumed by the session
// host (`apply_extension_notifications`) or the TUI drain — one path, no
// dead seams.

/// Configuration for a new runtime.
#[derive(Debug, Clone)]
pub struct ExtensionConfig {
    pub cwd: PathBuf,
    pub mode: String,
    pub has_ui: bool,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub system_prompt: String,
    /// Construction-time idle flag kept for host configuration/validation
    /// (the print host passes `true`). `ctx.isIdle()` no longer reads it —
    /// the host fn answers from the live session streaming cell (see
    /// `SessionContextSnapshot` consumers), so run transitions are visible.
    pub is_idle: bool,
    /// QuickJS resource limits for this runtime.
    ///
    /// `None` (the host default) makes the runtime resolve them itself: the
    /// GLOBAL settings file (`~/.pi/agent/settings.json`) overridden by the
    /// `PI_EXT_*` environment variables, then the built-in defaults — project
    /// settings are deliberately ignored (a repository must not be able to
    /// loosen the host's engine limits). `Some(limits)` is adopted verbatim
    /// (no settings/env overlay): the SDK/embedder/test injection seam. See
    /// [`crate::extensions::limits`].
    pub limits: Option<crate::extensions::limits::ExtensionLimits>,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            mode: "headless".to_string(),
            has_ui: false,
            model: None,
            session_id: None,
            system_prompt: String::new(),
            is_idle: true,
            limits: None,
        }
    }
}

impl ToolDef {
    /// Def for a host-pushed core (builtin) tool: the real description and
    /// parameter schema, so the JS `getActiveTools`/`getAllTools` facades see
    /// what upstream sends for builtins (a `parameters: null` placeholder left
    /// JS consumers — e.g. the tool-repair extension's live-schema pass —
    /// unable to repair calls to core tools).
    ///
    /// `executionMode` stays `"parallel"`: the hardcoded value the JS facades
    /// saw for core tools before they carried schemas, deliberately unchanged
    /// here (the per-tool execution mode of core tools is not modelled in the
    /// host-pushed def; plugin rows carry theirs from the plugin registry).
    pub(crate) fn core(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            label: None,
            description: description.into(),
            prompt_snippet: None,
            prompt_guidelines: Vec::new(),
            parameters,
            output_schema: JsonValue::Null,
            source_info: Default::default(),
            execution_mode: "parallel".to_owned(),
            exposure: String::new(),
            namespace: None,
            annotations: None,
            default_active: true,
            prepare_loadout: false,
            reaches_tools: None,
        }
    }
}
