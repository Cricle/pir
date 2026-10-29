//! Host side of the extension runtime: load, hook dispatch, registries.
//!
//! [`ExtensionRuntime`] owns the QuickJS runtime/context, injects the host
//! function table (`__pir_host.*`) and the JS prelude, and exposes the
//! load/dispatch APIs the pir session loop calls. All JS values stay inside
//! QuickJS; the Rust/JS boundary is JSON strings.

#[cfg(feature = "extensions")]
mod exec;
#[cfg(feature = "extensions")]
mod host_functions;
#[cfg(feature = "extensions")]
pub(crate) mod host_op_stats;

use std::collections::HashMap;
#[cfg(feature = "extensions")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

#[cfg(feature = "extensions")]
use rquickjs::promise::MaybePromise;
#[cfg(feature = "extensions")]
use rquickjs::{AsyncContext, AsyncRuntime, Error as JsError};
#[cfg(feature = "extensions")]
use serde::Deserialize;
use serde_json::Value as JsonValue;

#[cfg(feature = "extensions")]
use self::host_functions::register_host_functions;
#[cfg(feature = "extensions")]
use super::api::{HOST_PRELUDE, HOST_PRELUDE_MODULE};
// The configurable QuickJS resource limits (always compiled; the engine
// applies them, `RuntimeState` carries the resolved value).
use super::limits::ExtensionLimits;
// `LoadedExtension` / `ExtensionLoadError` are data types: they arrive through
// the `crate::extensions::types::*` glob re-export below.
#[cfg(feature = "extensions")]
use super::loader::{discover_extensions, ExtensionModuleLoader, SourceCache};
// Submodule files resolve `super::transpile` / `super::loader` through these
// bindings (they were sibling paths when this code lived in mod.rs).
#[cfg(feature = "extensions")]
use super::{loader, transpile};

/// File-backed shim assets embedded below this module (the worker boot
/// overlay), aggregated by `api::all_shims` for the byte-identity test.
#[cfg(feature = "extensions")]
pub(crate) fn worker_shim_manifest(
) -> &'static [(&'static str, &'static super::api::embedded::Shim)] {
    host_functions::worker_fns::SHIMS
}

// `DEFAULT_CALL_TIMEOUT` / `HOOK_CALL_TIMEOUT` live with the data types in
// `crate::extensions::types`; the glob re-export below brings them into scope.

/// Bound for interactive `ctx.ui.*` dialog waits (the JS side resolves
/// `null` once it passes) and for the expiry sweep of dialog replies and
/// waiter registrations (P2-9).
pub(crate) const DIALOG_TIMEOUT: Duration = Duration::from_secs(600);

/// Grace period between a host command arriving during a *background* JS job
/// and the interrupt that makes the actor yield to it. Small enough that a
/// waiting prompt never notices (the measured pi-subagents advertisement scan
/// would otherwise hold the actor for minutes), large enough that a
/// continuation one microtask from finishing is not needlessly aborted.
pub(crate) const BACKGROUND_PREEMPT_GRACE: Duration = Duration::from_millis(100);

/// Monotonic milliseconds since the first call in this process (the
/// `AtomicU64` representation of a preemption instant; `Instant` itself is
/// not atomic).
pub(crate) fn monotonic_millis() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

thread_local! {
    /// Stack of dispatch epochs for the JS call currently executing on this
    /// (actor) thread. A suspended dispatch keeps its entry on the stack
    /// while another interleaved call pushes/pops its own, so a host function
    /// called from JS always observes the epoch of the innermost active
    /// dispatch; calls made outside any dispatch observe `0`.
    static DISPATCH_EPOCH_STACK: std::cell::RefCell<Vec<u64>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The epoch of the dispatch whose JS is executing right now (`0` for
/// background continuations and host-thread calls).
pub(crate) fn current_dispatch_epoch() -> u64 {
    DISPATCH_EPOCH_STACK.with(|stack| stack.borrow().last().copied().unwrap_or(0))
}

/// Run one JS dispatch under `epoch`: every notification queued by its host
/// functions is tagged with it, so a caller that timed out can have its late
/// side effects dropped instead of applied to a later run. Epoch `0` is a
/// no-op wrapper (host-thread/background work is never abandoned this way).
#[cfg(feature = "extensions")]
pub(crate) async fn with_dispatch_epoch<F: std::future::Future>(
    epoch: u64,
    future: F,
) -> F::Output {
    struct PopEpoch;
    impl Drop for PopEpoch {
        fn drop(&mut self) {
            DISPATCH_EPOCH_STACK.with(|stack| {
                let _ = stack.borrow_mut().pop();
            });
        }
    }
    if epoch == 0 {
        return future.await;
    }
    DISPATCH_EPOCH_STACK.with(|stack| stack.borrow_mut().push(epoch));
    let _guard = PopEpoch;
    future.await
}

/// Process-wide count of constructed QuickJS extension runtimes (the engine +
/// prelude boot in `ExtensionRuntime::new_with_state`). Always compiled so a
/// build without the `extensions` feature can still answer `0`.
///
/// Seam for the builtin-only startup regression (`tests/builtin_only_no_js.rs`):
/// a run whose extension plan has no on-disk JS path must NOT construct a
/// runtime, so the count is unchanged across
/// [`crate::agent_session::load_extension_runtime`].
static JS_RUNTIME_BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Increment the [`JS_RUNTIME_BUILDS`] counter (called from the single
/// QuickJS construction path).
#[cfg(feature = "extensions")]
pub(crate) fn record_js_runtime_build() {
    JS_RUNTIME_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many QuickJS extension runtimes this process has constructed.
///
/// A builtin-only extension plan (only `builtin:<name>` paths) must leave
/// this unchanged: it runs on a plan-only [`crate::extensions::ExtensionHandle`]
/// with no engine. Regression seam for the ~10.6 MB builtin-only startup
/// cost; `0` in a build without the `extensions` feature.
#[doc(hidden)]
pub fn js_runtime_build_count() -> u64 {
    JS_RUNTIME_BUILDS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(feature = "extensions")]
mod dispatch;
// Bounded, ordered backlog for fire-and-forget extension dispatches (the
// per-chunk `provider_stream_event` hook): one worker per backlog, bounded
// queue, sync non-blocking `push_with`.
mod backlog;
// Extension-originated stdio routing (`process.stdout/stderr.write` off the
// TUI frame; see the module docs).
#[cfg(feature = "extensions")]
mod ext_stdio;
#[cfg(feature = "extensions")]
mod hooks;
#[cfg(feature = "extensions")]
mod ops;
#[allow(clippy::module_inception)]
// `runtime::runtime` holds the `ExtensionRuntime` impl split out of `mod.rs`
#[cfg(feature = "extensions")]
mod runtime;
mod state;

// The engine and the crate's unit tests are the callers; keep the
// re-export present in every configuration.
#[allow(unused_imports)]
pub(crate) use state::lock_state;
#[cfg(feature = "extensions")]
pub(crate) use state::recover_interrupted;
// Bounded fire-and-forget dispatch backlog (one worker task per instance, not
// per event); see the module docs and [`EventBacklog`].
pub use backlog::EventBacklog;
// The stack a `worker_threads` runtime requests of QuickJS. Public so the
// engine bound it must respect (rquickjs silently maps an oversized request
// to "no stack check"; see the constant's docs) is pinned by a test.
#[cfg(feature = "extensions")]
pub use host_functions::worker_fns::WORKER_JS_STACK_BYTES;
// The stdio routing rule (destination selection) is public so hosts/tests can
// state it directly; the write bridge stays crate-internal. See the module
// docs for the mode → destination → prefix table.
#[cfg(feature = "extensions")]
pub use ext_stdio::{ext_stdio_route, ExtStdioRoute, MAX_ROUTED_WRITE_BYTES};
// The script runner (`main_cli/js_mode.rs`) seeds the same sandbox root for
// a standalone script entry that `load` seeds for extension entries. The
// bounded rejection log is public so its eviction invariant is testable
// without a live QuickJS runtime.
/// A child wait's condvar park probe (blocking-host wakeup regression).
#[doc(hidden)]
#[cfg(feature = "extensions")]
pub use host_functions::child_fns::{
    activity_park_probe_for_tests, append_base64_chunk_for_tests, sync_drain_wait_slice_for_tests,
};
/// Test seams for the adaptive `workerRecvPoll` fallback and its idle-wait
/// probe (the worker registry/runtime stay private).
#[doc(hidden)]
#[cfg(feature = "extensions")]
pub use host_functions::worker_fns::{
    next_poll_slice_for_tests, poll_slice_bounds_for_tests, test_recv_poll, test_register_worker,
    test_release_worker, test_worker_poll_wakes, test_worker_send, worker_limit_for_tests,
};
/// Live process-level host handle counts (generation-release regression).
#[doc(hidden)]
#[cfg(feature = "extensions")]
pub use host_functions::{
    fs_watch_count_for_tests, host_resource_counts_for_tests, HostResourceCounts,
};
/// Test seams for the idle drain backoff (integration tests live in
/// `tests/`; inline test modules are forbidden in `src/`).
#[doc(hidden)]
#[cfg(feature = "extensions")]
pub use runtime::{next_park_slice_for_tests, park_slice_bounds_for_tests};
#[cfg(feature = "extensions")]
pub use runtime::{package_entry_root, RejectionLog};
// The `message_update` streaming wire (the delta-only per-delta wire built on
// the actor thread; see [`dispatch::message_update_stream_json`]) is public so
// the JS-contract and per-delta allocation tests can pin it against the
// reference construction
// ([`crate::agent::agent_loop::message_update_payload`]), together with the
// fallback snapshot serializer the JS accumulator uses for an unrecognised
// delta shape ([`dispatch::message_update_snapshot_json`]).
#[cfg(feature = "extensions")]
pub(crate) use dispatch::message_update_snapshot_json_for_partial;
#[cfg(feature = "extensions")]
pub use dispatch::{message_update_snapshot_json, message_update_stream_json};
// Definition/data types are engine-free and live in the always-compiled
// `crate::extensions::types` (SDK-SCOPE.md): re-exported here so the
// existing `runtime::<Type>` paths and the `use super::*` globs in this
// module's submodules keep resolving.
pub use crate::extensions::types::*;
/// Host-pushed `ctx.modelRegistry.complete` capability. Carries the resolved
/// session model identity the closure was built for plus the completion
/// closure itself ([`crate::compaction::CompleteFn`]); the JS-side op verifies
/// the requested model before running it (a mismatched model cannot be
/// completed through a closure built for another one — the honest failure is
/// a clear error, never a silently wrong request).
#[derive(Clone)]
#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
pub(crate) struct HostCompletion {
    pub(crate) provider: String,
    pub(crate) model_id: String,
    pub(crate) complete: Arc<crate::compaction::CompleteFn>,
}

#[cfg_attr(not(feature = "extensions"), allow(dead_code))]
pub(crate) struct RuntimeState {
    pub(crate) config: ExtensionConfig,
    pub(crate) invalidated: bool,
    /// Abort flag for the currently running tool call; polled by the JS side
    /// and set by [`ExtensionRuntime::abort_current_call`] / `ctx.abort()`.
    pub(crate) tool_call_aborted: bool,
    /// Extension ids registered with the QuickJS side (via `registerExtId`),
    /// used by the `isActive` host function to reject unknown ids.
    pub(crate) registered_ext_ids: std::collections::BTreeSet<i64>,
    /// Session name as written by `pi.setSessionName`; read back by
    /// `pi.getSessionName` (initialized from `config.session_id`).
    pub(crate) session_name: Option<String>,
    /// Per-call JS dispatch timeout. Enforced twice: as a tokio deadline
    /// around the call AND as a QuickJS interrupt-handler deadline that
    /// actually aborts a runaway JS job, leaving the runtime usable.
    /// Mutable at runtime via
    /// [`crate::extensions::ExtensionHandle::set_call_timeout`]; initialized
    /// from the resolved [`ExtensionLimits`] (default [`DEFAULT_CALL_TIMEOUT`]).
    pub(crate) call_timeout: Duration,
    /// Deadline of reserved-prefix hook calls (`__pir_stream:` /
    /// `__pir_route:`): those keys route to a JS `streamSimple` / virtual-
    /// model `route` callback, where a real model backend may legitimately
    /// run for many minutes. Initialized from the resolved
    /// [`ExtensionLimits`] (default [`HOOK_CALL_TIMEOUT`]).
    pub(crate) hook_timeout: Duration,
    /// The resolved QuickJS resource limits this runtime was built with
    /// (explicit injection, or global settings + `PI_EXT_*` env + defaults).
    pub(crate) limits: ExtensionLimits,
    /// One-shot gate for the "JS heap is at N% of the configured limit"
    /// diagnostic: set when the warning fires, reset once memory usage drops
    /// back below the threshold — so a heap that hovers near the cap warns
    /// once per crossing, never once per call.
    pub(crate) memory_pressure_warned: bool,
    /// Last throttled heap sample (`memory_usage()` walks every allocation,
    /// so completed-call sampling is spaced out; see
    /// [`ExtensionRuntime::sample_memory_pressure`]).
    pub(crate) memory_pressure_last_sample: Option<std::time::Instant>,
    /// Current thinking level (updated by `pi.setThinkingLevel` and the
    /// session host snapshot).
    pub(crate) thinking_level: String,
    /// Host-pushed live session/model snapshot backing the `ctx` facade.
    ///
    /// `Arc`-shared with the plugin host that pushed it: the snapshot carries
    /// the serialized session entries (the whole transcript JSON), so a
    /// per-prompt deep copy just to hand the runtime the same value was the
    /// largest avoidable copy on the extension message bridge.
    pub(crate) session: Arc<SessionContextSnapshot>,
    /// Serialized scoped-model list (`ctx.scopedModels`).
    pub(crate) scoped_models: JsonValue,
    /// Serialized model registry snapshot (`ctx.modelRegistry`).
    pub(crate) model_registry: JsonValue,
    /// Host-pushed `BuildSystemPromptOptions` snapshot (upstream
    /// `before_agent_start.systemPromptOptions` / `ctx.getSystemPromptOptions`):
    /// the same normalized object the session's current system prompt was
    /// derived from. `Null` until the first prompt pushes one; the host fn
    /// falls back to the minimal `{cwd, customPrompt}` shape.
    pub(crate) system_prompt_options: JsonValue,
    /// Fingerprint of the session state the pushed options were derived from
    /// (live prompt text + declared tool set + MCP section). The session host
    /// recomputes the option object — which includes resource discovery — only
    /// when this changes, so a steady turn reuses the snapshot.
    pub(crate) system_prompt_options_key: String,
    /// Extension-registered provider definitions (`pi.registerProvider`),
    /// each `{id, config}` as provided. The session host converts these into
    /// real registry providers at the next prompt.
    pub(crate) provider_defs: Vec<JsonValue>,
    /// Extension-registered MCP servers (`pi.registerMcpServer`), each
    /// `{name, config}` as provided. The `mcp` builtin consumes them at the
    /// next registry rebuild (upstream `McpServerRegistry`).
    pub(crate) mcp_server_defs: Vec<JsonValue>,
    /// Extension-registered virtual-model definitions
    /// (`pi.registerVirtualModel`), each carrying `{...definition, source}`.
    /// The session host applies them to the provider registry's virtual-model
    /// overlay at the next prompt.
    pub(crate) virtual_model_defs: Vec<JsonValue>,

    pub(crate) flags: Vec<FlagDef>,
    pub(crate) flag_values: HashMap<String, FlagValue>,
    pub(crate) active_tools_override: Option<Vec<String>>,
    /// Host-pushed tool defs for the non-extension tools the JS facades must
    /// see: the session's core (builtin) tools, then every Rust-backend tool
    /// of the unified plugin registry (Rust builtins such as `subagent` /
    /// `tool_search` / `python` / `codemode`, plus MCP-discovered `mcp__*`
    /// rows) whose name no core def already covers — first name wins, CORE
    /// first, the same precedence [`crate::agent::tool_execution::merge_plugin_tools`]
    /// applies when the run registry is built. Defs, not bare names: the
    /// facades must expose the real description and parameter schema.
    /// JS-extension tools are NOT pushed here; they already arrive through
    /// [`Self::tool_defs`] (the QuickJS registry export).
    pub(crate) core_tools: Vec<ToolDef>,
    pub(crate) tool_defs: Vec<ToolDef>,
    /// Per-tool wire metadata the typed [`ToolDef`] does not model, keyed by
    /// tool name: the `reachesTools` marker (upstream
    /// `ToolDefinition.reachesTools`; the hook function itself stays in
    /// QuickJS, where `pi.getAllTools()` re-attaches it by name) and the raw
    /// `namespace` object carrying its `aliases` (upstream
    /// `ToolNamespace.aliases`). `getAllToolsJson` merges them back into the
    /// serialized definitions so `pi.getAllTools()` stays faithful.
    pub(crate) tool_extras: HashMap<String, JsonValue>,
    /// Memo for `getAllToolsJson`: the serialized tool-def array plus the
    /// fingerprint of the inputs it was built from ([`Self::core_tools`],
    /// [`Self::tool_defs`], [`Self::tool_extras`]). That host fn runs per
    /// tool-context creation and the array embeds every parameter schema, so
    /// the built string is cached; each call re-hashes the inputs (an
    /// allocation-free pass into a `DefaultHasher`, no JSON string built) and
    /// a mismatch — ANY change to the inputs, including a schema-only edit
    /// under an unchanged tool name — rebuilds the entry. A version counter
    /// bumped by the writers was rejected: the QuickJS registry export lives
    /// in a different module and writes `tool_defs`/`tool_extras` directly, so
    /// the memo validates against the data itself and can never serve a stale
    /// array.
    pub(crate) all_tools_json: Option<(u64, String)>,
    pub(crate) command_defs: Vec<CommandDef>,
    pub(crate) shortcut_defs: Vec<ShortcutDef>,
    /// Registered handler count per event name, refreshed with the registry
    /// and — counts-only — at the tail of every other JS entry point the actor
    /// runs ([`ExtensionRuntime::refresh_handler_counts`]); backs the
    /// fast-path `has_handlers` check.
    pub(crate) handler_counts: HashMap<String, usize>,
    /// Monotone generation counter of JS handler-table mutations: `pi.on(...)`
    /// registration and its unsubscribe splice (plus the `refreshModels` row
    /// `pi.registerProvider` installs directly) each bump it through the
    /// `handlersDirty` host function. A plain `u64` inside this
    /// mutex-guarded state — read and written under the existing lock, one
    /// word per access, no new synchronization.
    pub(crate) handlers_gen: u64,
    /// The [`Self::handlers_gen`] value read BEFORE the export whose counts
    /// were installed as the current [`Self::handler_counts`]; written
    /// together with (and after) those counts.
    ///
    /// A generation, not a bool: the window between the JS export and the Rust
    /// install still allows a registration to land, so the snapshot needs a
    /// timestamp-like stamp rather than a "something changed" flag. With the
    /// pair `(counts, gen@export)` a synchronous reader on any thread can
    /// decide staleness — `handlers_gen != handlers_snapshot_gen` means a
    /// mutation landed after that export and the counts may miss it, so
    /// `has_handlers` conservatively answers `true`: only the dirty window
    /// pays (one emission round-trip that may find no handler — harmless),
    /// while equal generations mean the fast path costs exactly what it did
    /// before (the counts lookup plus two word loads).
    pub(crate) handlers_snapshot_gen: u64,
    /// Monotone generation counter of JS registry mutations: every
    /// `register*`/`unregister` path that can change the JSON produced by
    /// `__pir_exportRegistryJson` (tools, commands, shortcuts, renderer
    /// buckets, `setWidget` factories, load discard) bumps it through the
    /// `registryDirty` host function. [`ExtensionRuntime::refresh_registry`]
    /// early-outs while
    /// `registry_gen == registry_snapshot_gen`: the expensive export (every
    /// tool schema, command, shortcut and renderer serialized and re-parsed)
    /// would republish exactly what the last export installed.
    pub(crate) registry_gen: u64,
    /// The [`Self::registry_gen`] value read BEFORE the export whose registry
    /// snapshot is currently installed; written together with (and after)
    /// that snapshot. Unequal generations mean a JS registration landed after
    /// the snapshot was taken, so the next `refresh_registry` must re-export
    /// (conservative: a bump can only cause an extra refresh, never a missed
    /// registration).
    pub(crate) registry_snapshot_gen: u64,
    /// Monotone generation counter of every write to a definition class the
    /// plugin host's registry gate fingerprints (`JsDefFingerprints`: JS tool
    /// names, command name+description, flag, shortcut, provider, MCP-server
    /// and virtual-model definitions).
    ///
    /// Deliberately DISTINCT from [`Self::registry_gen`]: the classes written
    /// straight through host fns (flags, providers, MCP servers, virtual
    /// models) never appear in the `__pir_exportRegistryJson` output, so
    /// moving `registry_gen` for them would force that expensive re-export for
    /// nothing. Conversely the JS-registry classes (tools, commands,
    /// shortcuts) move both counters: `registry_gen` to re-export, and this
    /// one so the host's gate knows the exported definitions may have moved
    /// (the commit of the export moves it too, closing the window between the
    /// JS write and the export that actually publishes it).
    ///
    /// The only consumer is the host-side `JsDefGenerationProbe`
    /// (`crate::plugins::host::JsDefGenerationProbe`): a moved value makes the
    /// host re-read and compare the fingerprints on its next emit, while an
    /// unchanged value proves no definition write happened since the host's
    /// committed snapshot, so the host skips that deep read. Like the other
    /// generations here it is a plain `u64` inside the mutex-guarded state:
    /// read and written under the existing lock, one word per access, no new
    /// synchronization.
    pub(crate) defs_gen: u64,
    /// Paths of the last `load` call (re-used by `/reload`).
    pub(crate) load_paths: Vec<PathBuf>,
    /// Whether a `/reload` is currently running (double-load guard).
    pub(crate) reloading: bool,
    /// Queued side effects tagged with the dispatch epoch that produced them
    /// (`0` = no dispatch in flight, e.g. a background continuation): an
    /// epoch whose dispatch the caller abandoned is filtered out instead of
    /// reaching a later run.
    pub(crate) notifications: Vec<(u64, Notification)>,
    /// Merged effective settings snapshot (upstream `getSettings()`); pushed
    /// by the session host. `Null` when no host pushed one.
    pub(crate) settings: JsonValue,
    /// Builtin (Rust plugin) commands pushed by the session host so
    /// `getCommands()` reports them alongside extension commands.
    pub(crate) builtin_commands: Vec<CommandDef>,
    /// Provider ids with a usable credential (session-host-pushed); backs the
    /// `pi.setModel` "no credential" `false` result.
    pub(crate) authenticated_providers: Vec<String>,
    /// Nested tool executors bound per in-flight tool call id (upstream
    /// `ctx.executeTool` round-trip). Bound by [`crate::agent::tool_execution::ExtensionTool`]
    /// for the duration of a call, consumed by the `executeTool` host fn.
    ///
    /// Each entry is an `Arc` so a lookup (`.get(id).cloned()`) is an O(1)
    /// refcount bump, no allocation: the `executeTool` host fn must hold the
    /// executor across its `.await` without keeping the state lock, and a
    /// bare [`crate::agent::nested_tool_calls::NestedToolExecutor`] clone
    /// would re-allocate its `caller_id` `String` on every nested call.
    pub(crate) nested_executors:
        HashMap<String, Arc<crate::agent::nested_tool_calls::NestedToolExecutor>>,
    /// Pending dialog replies (`dialog id` -> (value, inserted_at, ttl)),
    /// written by the TUI via `ExtensionHandle::respond_dialog` and awaited
    /// by the dialog host functions. `ttl` is the waiter's effective wait
    /// (request-level `timeout` when it had one, else the global dialog TTL)
    /// and drives the expiry sweep (P2-9): entries whose waiter died are
    /// removed once older than that wait instead of accumulating.
    pub(crate) dialog_responses: HashMap<String, (JsonValue, std::time::Instant, Duration)>,
    /// Per-dialog wait notifications (P1-3): `dialog id` ->
    /// (notify, created_at, waiter ttl). One Notify per dialog id, so a
    /// reply wakes exactly its own waiter — a shared `notify_one` could wake
    /// the wrong waiter, whose re-check would consume the permit and stall
    /// the answered dialog's JS await for up to the dialog timeout. The
    /// per-waiter ttl (request-level `timeout` overrides the global dialog
    /// TTL) keeps the expiry sweep from evicting a LIVE waiter just because
    /// the global TTL is shorter (upstream `opts.timeout` semantics,
    /// types.ts:117-118).
    pub(crate) dialog_notifies: HashMap<
        String,
        (
            std::sync::Arc<tokio::sync::Notify>,
            std::time::Instant,
            Duration,
        ),
    >,
    /// TTL for dialog replies / waiters (test-injectable via
    /// `ExtensionHandle::set_dialog_ttl`; default [`DIALOG_TIMEOUT`]).
    pub(crate) dialog_ttl: Duration,
    /// Shared `ctx.ui.getToolsExpanded` / `setToolsExpanded` cell (upstream
    /// types.ts:296-299). The TUI holds a clone for tool-call rendering and
    /// its Ctrl+O toggle, so one `Arc<AtomicBool>` is the single source of
    /// truth for both faces: an extension toggle restyles the live transcript
    /// on the next frame, and a TUI toggle is visible to the next
    /// `getToolsExpanded()` call. Exactly one cell per runtime (never a
    /// second JS-local copy).
    pub(crate) tools_expanded: Arc<std::sync::atomic::AtomicBool>,
    /// Number of live `ctx.ui.onTerminalInput` handlers in the JS prelude
    /// (`state.terminalInputHandlers`). Kept as an `AtomicUsize` shared with
    /// the public [`ExtensionHandle`](crate::extensions::ExtensionHandle) so
    /// the TUI's per-input fast path is one atomic load and never takes the
    /// runtime state lock. The prelude publishes every register/unregister
    /// through the `terminalInputHandlers` host function.
    pub(crate) terminal_input_handlers: Arc<std::sync::atomic::AtomicUsize>,
    /// Renderer registrations exported from the JS registry
    /// (`registerMessageRenderer` / `registerEntryRenderer` /
    /// `registerMarkdownTransformer` / `registerToolRenderer`).
    pub(crate) renderer_defs: JsonValue,
    /// Monotonic dialog correlation id counter.
    pub(crate) next_dialog_id: u64,
    /// Host-pushed completion capability backing `ctx.modelRegistry.complete`
    /// for providers the host executes (`__pir_host.modelCompleteJson`). The
    /// session host pushes `AgentSession::complete_fn_for(current model)` on
    /// every prompt ([`crate::agent_session::AgentSession::push_session_context`]);
    /// `None` (headless/script hosts, or a host that never pushed) makes the
    /// op fail closed with a clear error.
    pub(crate) host_completion: Option<HostCompletion>,
    /// Live editor text snapshot pushed by the TUI host (backing the sync
    /// `ctx.ui.getEditorText()`); host-pushed state, survives reloads.
    pub(crate) editor_text: String,
    /// Host-pushed theme registry backing `ui.getThemes()` / `ui.getTheme` /
    /// `ui.setTheme`: `{themes: [{name, path}], current: name,
    /// snapshots: {name: snapshot}}`. Host-pushed state, survives reloads.
    pub(crate) theme_registry: JsonValue,
    /// Whether any `ui.addAutocompleteProvider` suggester is registered
    /// (fast-path check before paying the async completion round-trip).
    pub(crate) has_autocomplete_suggesters: bool,
    /// Bound streamSimple options-hook bridges (upstream `onPayload` /
    /// `onResponse` / `onProviderStreamEvent`), keyed by the per-request
    /// `hookSession` id the JS-stream provider puts into the callback's
    /// `options` object. Bound before a stream starts, unbound when it ends;
    /// `__pir_host.streamHookEvent` fires the matching observer.
    pub(crate) stream_hook_bridges: HashMap<String, StreamHookBridge>,
    /// Open file descriptors of the fs v2 surface (`openSync` family): OS
    /// handles plus host-tracked sequential offsets. The runtime clears the
    /// table at teardown (`ExtensionRuntime::drop`) since the state itself
    /// is shared into a reload's fresh runtime.
    pub(crate) fd_table: state::FdTable,
    /// Budget for the `before_agent_start` lifecycle dispatch
    /// (`extensionLimits.beforeAgentStartTimeoutMs` /
    /// `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS`, default
    /// [`super::limits::DEFAULT_BEFORE_AGENT_START_TIMEOUT`]). The handle
    /// applies it to that event's caller-side wait; a timed-out dispatch is
    /// abandoned (reply discarded, queued side effects dropped) so the run
    /// proceeds.
    pub(crate) before_agent_start_timeout: Duration,
    /// Commands the handle queued and the actor has not completed yet. The
    /// QuickJS interrupt handler consults it (together with
    /// [`Self::commands_in_flight`]) to decide whether a *background* JS job
    /// (a promise continuation left behind by a finished command) must yield
    /// to a queued host command.
    pub(crate) pending_commands: Arc<AtomicUsize>,
    /// Command tasks currently executing on the actor. While one is in
    /// flight the background-preemption rule is off: the executing dispatch
    /// owns its own armed deadline.
    pub(crate) commands_in_flight: Arc<AtomicUsize>,
    /// Monotonic-millisecond instant after which a background JS job is
    /// interrupted because a host command is waiting; `0` = no preemption
    /// requested. Written by the handle before it sends a command (a small
    /// grace lets an almost-finished continuation run to completion), read by
    /// the QuickJS interrupt handler.
    pub(crate) preempt_background_at_ms: Arc<AtomicU64>,
    /// Set by the actor's drive loop when it interrupted a background job;
    /// the next command task recovers the runtime (clears the pending
    /// interrupt exception) before running its JS.
    pub(crate) background_interrupted: Arc<AtomicBool>,
    /// Dispatch epochs whose caller stopped waiting (host-side timeout). Any
    /// side effect (notification) pushed by that dispatch after the timeout
    /// is dropped instead of being applied to a later run; the actor clears
    /// the entry when the abandoned dispatch finally completes.
    pub(crate) abandoned_dispatch_epochs: Arc<StdMutex<std::collections::BTreeSet<u64>>>,
    /// Monotone dispatch-epoch source shared with the handle (each `Emit`
    /// command gets one epoch).
    pub(crate) next_dispatch_epoch: Arc<AtomicU64>,
    /// Sanitized display identity (`node_modules/<pkg>/<entry>` or the last
    /// two path components) per loaded extension id, used by the
    /// `PI_TIMING=1` per-handler lines so those never print a full path.
    pub(crate) ext_identities: HashMap<u64, String>,
    /// Dispatch epochs of the handlers currently executing (JS pushes on
    /// entry, pops on settle). A handler continuation that resumes AFTER its
    /// dispatch returned still has its entry on this stack, so a late
    /// `pi.sendMessage` is tagged with the abandoned epoch and dropped
    /// instead of reaching a later run. The stack is per-runtime and JS is
    /// single-threaded, so a plain `Vec` under the state lock is enough.
    pub(crate) js_dispatch_epochs: Vec<u64>,
}
// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

#[cfg(feature = "extensions")]
pub struct ExtensionRuntime {
    ctx: AsyncContext,
    /// Handle to the QuickJS async runtime (used for pending-job recovery
    /// after an interrupt).
    js_runtime: AsyncRuntime,
    cache: SourceCache,
    allowed_roots: Arc<StdMutex<Vec<PathBuf>>>,
    /// This runtime's generation key: the same value stamped onto fs
    /// descriptors and every process-level host handle (children, workers,
    /// fetches, HTTP servers, net sockets), so teardown releases exactly this
    /// generation's resources (see `host_functions::release_host_generation`).
    generation: u64,
    state: Arc<StdMutex<RuntimeState>>,
    /// Extensions that loaded successfully (last `load` call). Interior-mutable
    /// so `load` can take `&self` and the actor can share the runtime as one
    /// `Arc` across spawned command tasks.
    loaded: StdMutex<Vec<LoadedExtension>>,
    next_ext_id: std::sync::atomic::AtomicU64,
    /// Armed interrupt deadlines keyed by call token (P1-4: overlapping
    /// timed calls each own their entry; the interrupt handler aborts JS
    /// once ANY armed deadline has passed, and a disarm removes only its
    /// own token — so an early-finishing call can never cancel another
    /// call's runaway-loop protection, and re-arming never overwrites an
    /// earlier deadline that is still armed).
    deadlines: Arc<StdMutex<HashMap<u64, std::time::Instant>>>,
    next_deadline_token: std::sync::atomic::AtomicU64,
    /// Promise rejections the engine reported still unhandled (the host
    /// promise-rejection tracker records reason texts; a later handler-attach
    /// event removes the entry). Read by the `pir js` script runner (Node's
    /// crash-on-unhandled-rejection contract); the extension host keeps
    /// collecting without ever acting on it — so the record is a BOUNDED
    /// [`runtime::RejectionLog`] (oldest reasons evicted past
    /// [`runtime::RejectionLog::MAX`]), never an unbounded `Vec` a rejection
    /// storm would grow for the process lifetime.
    unhandled_rejections: Arc<StdMutex<runtime::RejectionLog>>,
}
