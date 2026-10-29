//! pi-compatible JS extension runtime.
//!
//! Loads TypeScript/JavaScript extensions (upstream `pi` extension format:
//! a module whose default export is a factory receiving the `pi` API object)
//! inside QuickJS, with host-provided virtual modules for the upstream
//! package imports and a minimal TypeBox shim.
//!
//! Layout:
//! - [`types`] — engine-free definition/data types (`ToolDef`, `Notification`,
//!   `ToolResult`, ...). Always compiled: a minimal SDK build keeps the whole
//!   plugin/hook data surface with the JS engine trimmed.
//! - [`transpile`] — swc-based TypeScript stripping.
//! - [`esm_cjs`] — swc-based ESM→CJS transform backing `require(esm)`.
//! - [`loader`] — extension discovery and the QuickJS module loader
//!   (virtual modules + extension files + relative imports).
//! - [`api`] — the JS-side `pi` object / `ctx` object / dispatch glue that is
//!   injected into every QuickJS runtime, plus the virtual module shims.
//! - [`runtime`] — the host-side [`runtime::ExtensionRuntime`]: load, hook
//!   dispatch, tool/command/flag registries, stale protocol.
//! - [`limits`] — the configurable QuickJS resource limits
//!   ([`limits::ExtensionLimits`]): global-settings + env resolution and the
//!   actionable out-of-memory text; engine-free like [`types`].
//!
//! Everything below [`types`] is the engine and sits behind the `extensions`
//! cargo feature (rquickjs + the swc transpile stack). See SDK-SCOPE.md.

#[cfg(feature = "extensions")]
pub mod api;
#[cfg(feature = "extensions")]
pub mod esm_cjs;
pub mod handle;
pub mod limits;
#[cfg(feature = "extensions")]
pub mod loader;
/// Floating native custom overlays (`ctx.ui.custom` with `overlay: true`):
/// typed option bag, the native `OverlayHandle`, and the host bounds-publish
/// seam. Engine-free: the JS face keeps its prelude implementation.
pub mod overlay;
/// Extension-decided project trust (`project_trust`): the process-wide
/// decision record plus the upstream resolution order. Engine-free.
pub mod project_trust;
/// Extension-contributed resource paths (`resources_discover`, upstream
/// `ResourceExtensionPaths` + `DefaultResourceLoader.extendResources`).
/// Engine-free: the path grammar and merge rules are shared by the session
/// host and the SDK resource loader.
pub mod resources;
pub mod runtime;
/// The full upstream `BuildSystemPromptOptions` exposed to
/// `before_agent_start` handlers (`systemPromptOptions`) plus the renderer
/// that turns mutated options back into this run's system prompt.
/// Engine-free.
pub mod system_prompt_options;
#[cfg(feature = "extensions")]
pub mod transpile;
pub mod types;
/// Execution path for custom `BashOperations` returned by `user_bash`
/// handlers (upstream `UserBashEventResult.operations`). Engine-free: the
/// executor drives the runtime through [`ExtensionHandle`].
pub mod user_bash;

pub use handle::ExtensionHandle;
pub use limits::{
    ExtensionLimits, DEFAULT_BEFORE_AGENT_START_TIMEOUT, ENV_BEFORE_AGENT_START_TIMEOUT_MS,
    ENV_CALL_TIMEOUT_MS, ENV_GC_THRESHOLD_MB, ENV_HOOK_TIMEOUT_MS, ENV_MEMORY_MB, ENV_STACK_MB,
};
#[cfg(feature = "extensions")]
pub use loader::{
    discover_extensions, scan_dependencies, DependencyKind, DependencyRef, ExtensionLoadError,
    LoadedExtension,
};
pub use overlay::{
    NativeOverlayAnchor, NativeOverlayBounds, NativeOverlayHandle, NativeOverlayMargin,
    NativeOverlayOptions, NativeOverlaySize,
};
#[cfg(feature = "extensions")]
pub use runtime::{ext_stdio_route, ExtStdioRoute, MAX_ROUTED_WRITE_BYTES};
#[cfg(feature = "extensions")]
pub use runtime::{ExtensionRuntime, RejectionLog};
#[cfg(feature = "extensions")]
pub use transpile::transpile;
pub use types::{
    CommandDef, CommandOutcome, ContentItem, EmitOutcome, EventError, ExtensionConfig, FlagDef,
    FlagValue, HookStreamItem, LoadResult, Notification, RendererCallOutcome, RendererOutcome,
    SessionContextSnapshot, ShortcutDef, SourceInfo, StreamHookBridge, StreamResponseHook,
    ToolAnnotationsDef, ToolDef, ToolNamespaceDef, ToolReachDef, ToolRenderShell, ToolResult,
    UserBashOutcome, DEFAULT_CALL_TIMEOUT, HOOK_CALL_TIMEOUT,
};
