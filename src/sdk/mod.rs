//! # pir SDK
//!
//! The curated, stable facade for embedding pir: build a custom agent in a
//! few lines, drive it from Rust, and extend it with typed tools and the
//! unified plugin system. It is a thin layer over the crate's internals
//! ([`crate::agent_session::AgentSession`], the agent loop, the provider
//! registry, and the plugin host) — not a rewrite — with a deliberately
//! small surface: [`AgentBuilder`], [`Agent`], [`SdkEvent`],
//! [`AgentOutcome`], and [`SdkError`].
//!
//! ## Getting started
//!
//! An agent needs a model, optionally tools, and optionally session
//! storage. The quickest path is an explicit endpoint (it needs the matching
//! `provider-*` cargo feature):
//!
//! ```no_run
//! use pir::agent::tool_batch::ToolResult;
//! use pir::sdk::{AgentBuilder, ApiKind, Endpoint};
//!
//! # async fn example() -> Result<(), pir::sdk::SdkError> {
//! let agent = AgentBuilder::new()
//!     .endpoint(Endpoint::new(
//!         "http://localhost:11434/v1",
//!         ApiKind::OpenAiCompletions,
//!         "llama3",
//!     ))
//!     .system_prompt("You are a helpful coding assistant.")
//!     .tool(
//!         "list_files",
//!         "List the files of a directory",
//!         serde_json::json!({
//!             "type": "object",
//!             "properties": { "path": { "type": "string" } },
//!             "required": ["path"]
//!         }),
//!         |args| async move {
//!             let path = args["path"].as_str().unwrap_or(".");
//!             // Real implementations would do bounded IO here.
//!             ToolResult::ok(format!("files in {path}: (example)"))
//!         },
//!     )
//!     .build()
//!     .await?;
//!
//! let outcome = agent.prompt("What files are in this project?").await?;
//! println!("{}", outcome.message_text().unwrap_or_default());
//! # Ok(())
//! # }
//! ```
//!
//! Without [`AgentBuilder::endpoint`], the builder resolves a model the
//! same way the pi CLI does: `~/.pi/agent/models.json`, the built-in
//! provider catalog, and configured credentials (env vars or
//! [`pir auth`](crate::ai::auth)) — see [`AgentBuilder::model`].
//!
//! For a self-contained example that works in a minimal build — a scripted
//! mock [`Provider`](crate::ai::provider::Provider) supplied through
//! [`AgentBuilder::with_registry`], one tool, and
//! [`Agent::prompt_stream`] — see `docs/sdk.md` and the runnable examples
//! under `examples/` (`quickstart`, `streaming`, `tools`, `sessions`,
//! `plugin`, `control`, `builtin`, `extension`, `proxy`, `real_endpoint`).
//!
//! ## Streaming
//!
//! [`Agent::prompt_stream`] yields typed [`SdkEvent`]s through a bounded
//! channel. Intermediate events are best-effort under consumer lag (a full
//! channel drops the event instead of stalling the agent loop); the terminal
//! [`SdkEvent::AgentSettled`] is delivered after the whole prompt run settles
//! and is never dropped for a live receiver. The stream always ends with it.
//!
//! ```no_run
//! # async fn example() -> Result<(), pir::sdk::SdkError> {
//! use futures_util::StreamExt;
//! use pir::sdk::{AgentBuilder, ApiKind, Endpoint, SdkEvent};
//!
//! let agent = AgentBuilder::new()
//!     .endpoint(Endpoint::new(
//!         "https://api.example.com/v1",
//!         ApiKind::AnthropicMessages,
//!         "my-model",
//!     ))
//!     .api_key("sk-...")
//!     .no_session()
//!     .build()
//!     .await?;
//! let mut events = agent.prompt_stream("Explain this module.");
//! while let Some(event) = events.next().await {
//!     match event {
//!         SdkEvent::MessageUpdate { delta, .. } => print!("{delta}"),
//!         SdkEvent::AgentSettled { .. } => break,
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Tools and plugins
//!
//! Two tool paths: [`AgentBuilder::tool`] (an async closure, the ergonomic
//! one-off) and [`AgentBuilder::with_tool`] (a full
//! [`AgentTool`](crate::agent::tool_execution::AgentTool) implementation).
//!
//! Three plugin paths, all landing on the same unified plugin host (one
//! registry and one ordered event dispatch, shared with JS extensions):
//! [`AgentBuilder::tool`] / [`AgentBuilder::with_tool`] for a tool,
//! [`AgentBuilder::with_plugin`] for an inline Rust
//! [`AgentPlugin`](crate::plugins::AgentPlugin), and [`AgentBuilder::builtin`]
//! / [`AgentBuilder::builtins`] for the compiled-in builtins by name
//! (`.builtin("subagent")`). JS extensions load through
//! [`AgentBuilder::extension_paths`].
//!
//! Each builtin family is its own cargo feature (`subagent`,
//! `tool-repair`, `tool-search`, `python`, `mcp`, `extensions` for codemode
//! and the JS runtime); a `.builtin(...)` name whose feature is off fails
//! [`AgentBuilder::build`] with the honest "not compiled into this build"
//! diagnostic, and an unknown name keeps the upstream unknown-name wording —
//! never a silent skip. See `docs/sdk.md` and `SDK-SCOPE.md` for the
//! trimming matrix.
//!
//! Skills and prompt templates come from a
//! [`DefaultResourceLoader`](crate::sdk::DefaultResourceLoader) attached with
//! [`AgentBuilder::resource_loader`] (or the lower-level
//! [`CreateAgentSessionOptions::resource_loader`]); without a loader the SDK
//! performs no implicit discovery.
//!
//! Event subscription: [`Agent::on_session_event`] delivers the out-of-loop
//! [`SessionEvent`]s (queue/compaction/transcript/retry/background) even when
//! the agent is driven with [`Agent::prompt`], which carries no
//! [`SdkEvent`] stream.
//!
//! ## Agent directory identity
//!
//! [`AgentBuilder::agent_dir`] gives an agent its own config directory — the
//! per-agent form of `PIR_AGENT_DIR` (D2), and the reason two agents with
//! DIFFERENT directories can live in one process:
//!
//! ```no_run
//! # async fn example() -> Result<(), pir::sdk::SdkError> {
//! let a = pir::sdk::AgentBuilder::new()
//!     .agent_dir("/tmp/agent-a")
//!     .no_session()
//!     .build()
//!     .await?;
//! let b = pir::sdk::AgentBuilder::new()
//!     .agent_dir("/tmp/agent-b")
//!     .no_session()
//!     .build()
//!     .await?;
//! assert_ne!(a.agent_dir(), b.agent_dir());
//! # Ok(())
//! # }
//! ```
//!
//! The directory is resolved ONCE, at [`AgentBuilder::build`] (the builder's
//! value, else the process environment exactly like the CLI), and it governs
//! every agent-dir-scoped read of that build: `settings.json`, `models.json`,
//! `auth.json`, the settings-driven builtin gates, the `subagent` builtin's
//! `<agent_dir>/agents` discovery directory, the `mcp` builtin's log /
//! OAuth store / global `mcp.json`, the `codemode` builtin's config, the
//! remote-catalog `models-store.json`, and (for
//! [`SessionMode::DefaultDir`]) the session root. [`Agent::agent_dir`]
//! reports it.
//!
//! Two limits are deliberate and documented:
//!
//! - **Build-bound.** A later `PIR_AGENT_DIR` change never re-points a live
//!   agent, and [`Agent::session_dir`] in particular stays where it was built:
//!   a session belongs to the directory and cwd it was created under (D9).
//!   Build a new agent to change either. [`Agent::attach_session`] still
//!   opens an explicit file anywhere.
//! - **Process-global switches stay global.** Environment variables
//!   (`PI_OFFLINE`, `PI_CATALOG_BASE_URL`, `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL`
//!   Claude Code compatibility, `PIR_CODEMODE_MEMORY_MB`, provider API-key
//!   variables) are not part of the directory identity: they describe the
//!   process, not the agent.
//!
//! ## Sessions
//!
//! By default the agent persists its transcript in the upstream JSONL
//! format under the default session directory (this needs the `session`
//! cargo feature; without it the transcript stays in memory and
//! [`Agent::session_path`] is `None`); [`AgentBuilder::session`] selects an
//! explicit directory or file, and [`AgentBuilder::no_session`] keeps
//! everything in memory. [`Agent::session_path`] / [`Agent::session_id`]
//! report the live session, and [`Agent::session_name`] /
//! [`Agent::set_session_name`] manage its display name. [`Agent::new_session`]
//! starts a fresh file (same runtime, tools, and plugins) and
//! [`Agent::attach_session`] resumes an existing file, replaying its
//! transcript for the next run (both require session storage and an idle
//! agent). [`Agent::session_dir`] reports the build-bound root new session
//! files land in.
//!
//! ## Prompting and introspection
//!
//! [`Agent::prompt`] / [`Agent::prompt_stream`] run text prompts;
//! [`Agent::prompt_with_images`] / [`Agent::prompt_stream_with_images`] attach
//! typed [`ImageContent`](crate::ai::types::ImageContent) blocks to the
//! starting user message. [`Agent::is_idle`] /
//! [`Agent::pending_message_count`] expose run state,
//! [`Agent::tools`] snapshots the merged tool registry, and
//! [`Agent::take_notifications`] drains plugin presentation notifications for
//! headless hosts.
//!
//! ## Session-level surface and managers
//!
//! Beyond the [`Agent`] facade, the SDK exposes the upstream embedding types:
//!
//! - [`AgentSession`] / [`SessionEvent`] / [`AgentSessionConfig`] — the raw
//!   session object (the same one the CLI hosts and the extension API use).
//!   [`SdkEvent::from_session_event`] projects a `SessionEvent` into the SDK
//!   event type, and an [`Agent`] prompt stream already carries those
//!   projections (`queue_update`, `compaction_start/_end`, `entry_appended`,
//!   `session_info_changed`, `thinking_level_changed`, `auto_retry_start/_end`,
//!   `bash_execution_update`, `background_tool_end`).
//! - [`create_agent_session`] / [`create_agent_session_services`] /
//!   [`create_agent_session_from_services`] / [`create_agent_session_runtime`]
//!   and [`AgentSessionRuntime`] — the factory/lifecycle surface
//!   (`new_session`, `switch_session`, `reload`, `dispose`).
//! - [`ModelRegistry`] (model lookup + configured-auth filtering),
//!   [`SessionManager`], [`SettingsManager`] / [`Settings`],
//!   [`DefaultResourceLoader`] (`ResourceLoader`, including
//!   `resources_discover` contributions), and [`DefaultPackageManager`]
//!   (installed-package resource paths).
//! - Tool factories: [`create_coding_tools`], [`create_read_only_tools`],
//!   [`create_read_tool`], [`create_bash_tool`], [`create_edit_tool`],
//!   [`create_write_tool`], [`create_ls_tool`], [`create_grep_tool`],
//!   [`create_find_tool`], [`create_powershell_tool`]. They hand back the
//!   exact objects the CLI registers and fail with the missing-feature
//!   diagnostic when a `tool-*` cargo feature is off.
//! - [`EventBus`] / [`create_event_bus`] — a native, typed publish/subscribe
//!   bus (upstream `core/event-bus.ts`).
//! - [`parity`] — the upstream `index.ts` parity surface: model-resolution
//!   diagnostics (`ModelRuntime`, `ModelScopeDiagnostic`,
//!   `resolve_model_scope`), the file mutation queue
//!   (`with_file_mutation_queue`), image utilities (`resize_image`),
//!   print/interactive/RPC run modes, the `RpcClient`, and theme/keybinding
//!   helpers. Items with no honest Rust equivalent are listed as not exported
//!   in that module's documentation rather than stubbed.
//!
//! Known gaps (documented, not faked):
//!
//! - `ctx.reload()` from an EXTENSION COMMAND is applied by the host session
//!   command plane (`AgentSession::reload`); [`AgentSessionRuntime::reload`]
//!   is the direct Rust equivalent (it also returns the resource report).
//! - `newSession`/`fork`/`switchSession` `withSession` callbacks now run from
//!   JS (a runtime token plus the host callback bridge); Rust embedders still
//!   run their own setup code after the corresponding runtime/`AgentSession`
//!   call, which is the natural Rust form.
//! - The "restore the model recorded on an existing session" step belongs to
//!   [`Agent::attach_session`]; the session factories build fresh sessions.
//!
//! ## Proxied requests
//!
//! [`stream_proxy`] / [`ProxyProvider`] route model requests through an
//! application server that owns the provider credentials — the Rust
//! equivalent of upstream's `streamProxy` export (POST to
//! `{proxyUrl}/api/stream` with a bearer token, SSE proxy events decoded with
//! client-side partial reconstruction). [`AgentBuilder::proxy`] is the
//! one-call builder path.
//!
//! ## Errors and shutdown
//!
//! Every fallible SDK entry point returns [`SdkError`] — a typed,
//! `#[non_exhaustive]` surface over the internal error paths. Teardown is
//! explicit: [`Agent::shutdown`] aborts in-flight runs and fires plugin
//! shutdown hooks; dropping the handle performs no async work.

pub mod agent;
pub mod builder;
pub mod error;
pub(crate) mod event;
pub mod event_bus;
pub mod outcome;
pub mod packages;
pub mod parity;
pub mod proxy;
pub mod registry;
pub mod resources;
pub mod runtime;
pub mod tools;

// The session facade and its event surface (upstream `AgentSession`,
// `AgentSessionEvent`, `AgentSessionEventListener`).
pub use crate::agent_session::{
    AgentSession, AgentSessionConfig, ClearedQueue, SessionEvent, SessionEventHandler,
};
// The upstream manager types the session factories accept/return.
pub use crate::compaction::CompactionResult;
pub use crate::config::settings::{FollowUpMode, Settings, SettingsManager, SteeringMode};
pub use crate::extensions::{CommandOutcome, Notification};
pub use crate::session_manager::{PagedWindow, SessionManager};
// SDK-specific session-level surface (upstream `createAgentSession*`,
// `AgentSessionRuntime`, `ResourceLoader`, package/model managers).
pub use agent::{Agent, ModelRef, ToolInfo, UsageSnapshot};
pub use builder::{AgentBuilder, ApiKind, Endpoint, SessionMode};
pub use error::SdkError;
pub use event::SdkEvent;
pub use event_bus::{create_event_bus, EventBus, EventBusSubscription};
pub use outcome::AgentOutcome;
pub use packages::DefaultPackageManager;
// Upstream `index.ts` parity surface (see `parity`). The core items are gated
// by the `sdk` feature alone; each optional item below keeps the cargo feature
// of the implementation it wraps.
#[cfg(any(feature = "print", feature = "tool-read"))]
pub use parity::detect_supported_image_mime_type_from_file;
#[cfg(feature = "rpc")]
pub use parity::run_rpc_mode;
pub use parity::{
    collect_settings_diagnostics, default_model_per_provider, find_exact_model_reference_match,
    find_initial_model, format_size, parse_thinking_level, resolve_cli_model,
    resolve_cli_model_with_auth, resolve_model_reference, resolve_model_scope, truncate_head,
    truncate_tail, with_file_mutation_queue, AgentSessionRuntimeDiagnostic, ContextUsage,
    InitialModelOptions, InitialModelResult, ModelCycleResult, ModelReferenceRequest,
    ModelReferenceResult, ModelReferenceTarget, ModelRuntime, ModelScopeDiagnostic, MutationQueues,
    ResolveCliModelResult, ResolveModelScopeResult, ResourceKey, ScopedModel, SessionStats,
    SessionTokens, SettingsDiagnostic, SettingsDiagnosticType, ThinkingSuffixMode,
    TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};
#[cfg(feature = "tui")]
pub use parity::{
    format_key_text, format_read_line_range, get_language_from_path, get_markdown_theme,
    highlight_code, init_theme, key_hint, key_text, raw_key_hint, run_interactive_mode, Appearance,
    InteractiveModeOptions, Keybindings, KeybindingsManager, MarkdownTheme, Theme, ThemeAppearance,
    ThemeColor, ThemeToken, TuiAction,
};
#[cfg(all(
    any(feature = "image-tools", all(feature = "print", feature = "tui")),
    any(feature = "print", feature = "tool-read")
))]
pub use parity::{resize_image, ImageResizeError, ImageResizeOptions, ResizedImage};
#[cfg(feature = "print")]
pub use parity::{run_print_mode, OutputMode, PrintModeOptions, PrintOutcome};
#[cfg(feature = "client")]
pub use parity::{
    RpcClient, RpcClientError, RpcClientOptions, RpcCommand, RpcEventListener,
    RpcEventSubscription, RpcExtensionUIRequest, RpcExtensionUIResponse, RpcResponse,
    RpcSessionState, CLOSE_GRACE, EVENT_CHANNEL_CAPACITY, IDLE_TIMEOUT, REQUEST_TIMEOUT,
    STDERR_CAPTURE_LIMIT,
};
pub use proxy::{
    stream_proxy, ProxyEndpoint, ProxyProvider, ProxyStreamOptions, PROXY_CONNECTION_CLOSED_MESSAGE,
};
pub use registry::{ModelRegistry, PiEnvironment};
pub use resources::{DefaultResourceLoader, ResourceLoader};
pub use runtime::{
    create_agent_session, create_agent_session_from_services, create_agent_session_runtime,
    create_agent_session_services, AgentSessionReloadOutcome, AgentSessionRuntime,
    AgentSessionServices, CreateAgentSessionOptions, CreateAgentSessionResult,
    CreateAgentSessionRuntimeResult,
};
pub use tools::{
    create_bash_tool, create_coding_tools, create_edit_tool, create_find_tool, create_grep_tool,
    create_ls_tool, create_powershell_tool, create_read_only_tools, create_read_tool, create_tool,
    create_write_tool, has_core_tool,
};
// Resource types the new APIs return (upstream `ResourceLoader` types,
// `Skill`, `PromptTemplate`, `PackageResources`).
pub use crate::coding_agent::package_manager::PackageResources;
pub use crate::coding_agent::prompt_templates::PromptTemplate;
pub use crate::coding_agent::resources::DiscoveredResources;
pub use crate::extensions::project_trust::ProjectTrustDecision;
pub use crate::extensions::resources::{ExtensionResourcePaths, ExtensionResourceReload};
pub use crate::skills::SkillInfo;
// Per-tool lifecycle hooks (upstream `AgentLoopConfig.beforeToolCall` /
// `afterToolCall`): the hook bundle, its context/result payloads, and the
// call/result types the callbacks receive. Exported here so an embedder can
// install hooks through `AgentBuilder::tool_hooks` / `Agent::set_tool_hooks`
// without reaching into the core agent modules.
pub use crate::agent::tool_batch::{PreparedToolCall, ToolResult};
pub use crate::agent::tool_execution::{
    AfterToolCallResult, BeforeToolCallResult, ToolContext, ToolHooks, ToolResultContentBlock,
};
