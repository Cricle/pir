//! [`PluginHost`]: the unified plugin dispatch + registry owner.
//!
//! Hosts (the [`crate::agent_session::AgentSession`] facade) talk to ONE
//! object: ordered plugin list = enabled Rust builtins (in `builtin:<name>`
//! path-plan position) → the QuickJS JS-extensions adapter → inline Rust
//! factories. Event dispatch, registry resolution (tools/commands/flags/
//! shortcuts/providers/renderers), and merge semantics are identical for
//! both runtimes because both implement [`AgentPlugin`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;

use crate::extensions::{EmitOutcome, ExtensionHandle, SessionContextSnapshot, ToolResult};

use super::builtin::instantiate_builtin;
#[cfg(feature = "mcp")]
use super::builtin::mcp::McpPlugin;
use super::merge::MergedEmit;

/// The `mcp` builtin's shared manager handle (`mcp` feature) or an
/// uninhabited placeholder in a build without it.
#[cfg(feature = "mcp")]
pub type McpManagerHandle = Arc<crate::mcp::manager::McpManager>;
#[cfg(not(feature = "mcp"))]
pub type McpManagerHandle = std::convert::Infallible;
use super::plugin::context::{PluginContext, PluginIdentity};
use super::plugin::{
    AgentPlugin, CommandBackend, EventInterest, PluginEvent, PluginEventOutcome,
    RegistrationContext, ShortcutBackend, ToolBackend,
};
use super::registry::{PluginDiagnostic, PluginRegistry, RegisteredPlugin};
use super::services::{Subscription, TerminalInputAction, TerminalInputHandler};
use super::source::{builtin_name, PluginInfo};

// ---------------------------------------------------------------------------
// The QuickJS adapter: the whole JS extension runtime as ONE plugin
// ---------------------------------------------------------------------------

/// The embedded QuickJS runtime riding the [`AgentPlugin`] contract. The
/// existing `ExtensionHandle` actor is unchanged — it becomes the JS
/// adapter's engine; its `emit` performs the JS-internal handler dispatch
/// (load order, registration order, per-event merge), whose merged outcome
/// then joins the shared merge layer as one plugin result.
pub struct JsExtensionsPlugin {
    handle: ExtensionHandle,
    info: PluginInfo,
}

impl JsExtensionsPlugin {
    pub fn new(handle: ExtensionHandle) -> Self {
        Self {
            handle,
            info: PluginInfo::inline("js-extensions"),
        }
    }
}

impl AgentPlugin for JsExtensionsPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Import the JS runtime's current registry snapshot into the shared
    /// registry (JS tools/commands/flags/shortcuts/providers/MCP servers/
    /// virtual models). Called at build time and on every registry rebuild
    /// (JS handlers may register during `session_start`/commands).
    ///
    /// Every definition class imported here is fingerprinted by the host's
    /// `JsDefFingerprints` snapshot — the snapshot
    /// `PluginHost::refresh_registry_if_changed` gates rebuilds on. The two
    /// must stay in sync: a class imported below but absent from the
    /// fingerprint would let the host skip a rebuild for a change only this
    /// pass can publish. The same applies to the gate's O(1) half: once a
    /// [`crate::plugins::host::JsDefGenerationProbe`] is installed, every
    /// write to one of these classes must move its generation, or the
    /// unchanged-generation skip would hide a real change.
    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        for def in self.handle.tool_defs() {
            let execution_mode = match def.execution_mode.as_str() {
                "sequential" => crate::agent::tool_batch::ToolExecutionMode::Sequential,
                _ => crate::agent::tool_batch::ToolExecutionMode::Parallel,
            };
            ctx.register_tool_def(def, execution_mode);
        }
        for command in self.handle.command_defs() {
            ctx.register_command(&command.name, command.description.as_deref(), None);
        }
        for flag in self.handle.flags() {
            ctx.register_flag(
                &flag.name,
                &flag.flag_type,
                &flag.description,
                flag.default.clone(),
            );
        }
        for shortcut in self.handle.shortcuts() {
            ctx.register_shortcut(&shortcut.key, shortcut.description.as_deref(), None);
        }
        for provider in self.handle.provider_defs() {
            let Some(id) = provider.get("id").and_then(JsonValue::as_str) else {
                continue;
            };
            let config = provider.get("config").cloned().unwrap_or(JsonValue::Null);
            ctx.register_provider(id, config);
        }
        // `pi.registerMcpServer` definitions ride the same rebuild path; the
        // `mcp` builtin's registry consumes them (the shared registry snapshot
        // records them for the dirty check below).
        for def in self.handle.mcp_server_defs() {
            ctx.register_mcp_server(def);
        }
        for definition in self.handle.virtual_model_defs() {
            ctx.register_virtual_model(definition);
        }
        Ok(())
    }

    fn events(&self) -> EventInterest {
        // `All` here means "the runtime is always CONSULTED for dispatch"
        // (the adapter cannot know the per-event answer at declaration time),
        // NOT "every event has a handler". `PluginHost::has_handlers` relies
        // on that distinction: it excludes the adapter from the Rust-interest
        // scan and asks the runtime for the precise per-event answer instead.
        EventInterest::All
    }

    /// Identity for the host's adapter check ([`is_js_adapter`]): the one
    /// plugin whose `EventInterest::All` is a "consult the runtime" marker
    /// rather than a wildcard handler claim. Downcast consumers (the `mcp`
    /// / `subagent` seams) match on their own concrete type, so exposing the
    /// adapter here cannot collide with them.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn on_event<'a>(
        &'a self,
        event: PluginEvent<'a>,
    ) -> super::plugin::PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            // The event name is borrowed from `event.name` (a `&str`), not
            // cloned into an owned `String`: this runs once per event and the
            // name is only ever compared/forwarded. `event.payload` is
            // reborrowed by the call below, so it stays usable for the
            // `before_provider_request` write-back.
            let event_name = event.name.as_ref();
            // `before_agent_start` runs under its dedicated, shorter budget
            // (`extensionLimits.beforeAgentStartTimeoutMs`): the first prompt
            // must not wait for a lifecycle hook stuck behind extension
            // background work. Every other event keeps the per-call budget.
            let emitted = if event_name == "before_agent_start" {
                self.handle.emit_before_agent_start(event.payload).await
            } else {
                self.handle.emit(event_name, event.payload).await
            };
            match emitted {
                Ok(outcome) => {
                    // `handle.emit` already wrote the (possibly mutated)
                    // payload back through `event.payload`.
                    //
                    // `before_provider_request` is the one event whose JS
                    // merge REPLACES the payload by return value instead of
                    // mutating it in place (`currentProviderPayload = result`,
                    // `prelude.js.txt`, `currentProviderPayload = result` (line numbers drift — cite the symbol)): the runtime surfaces the
                    // final replacement as `outcome.result`, so without this
                    // write-back a later plugin would observe the
                    // pre-replacement body and its own result would silently
                    // discard the JS rewrite (`PluginHost::emit`'s "later
                    // handlers observe earlier rewrites" contract). The host
                    // still reads the replacement from `outcome.result`.
                    //
                    // Build the typed outcome FIRST (it only borrows
                    // `outcome.result`), then MOVE the replacement into the
                    // payload mirror. The old order cloned the whole provider
                    // body into the mirror and `from_wire` cloned it again;
                    // `before_provider_request` carries the complete request
                    // (system prompt + every message), so one of those deep
                    // copies was pure per-request overhead.
                    //
                    // `PluginEventOutcome::from_wire` reduces the
                    // `session_before_*` family to `{cancel}`, which would
                    // drop the fork-only `skipConversationRestore` before the
                    // session host (session_store.rs) can honor it. Carry it
                    // explicitly when it is the JS handlers' decision; a
                    // cancel still rides the typed SessionBefore variant so
                    // later plugins are short-circuited.
                    let typed = if event_name == "session_before_fork"
                        && outcome.result.get("cancel").and_then(JsonValue::as_bool) != Some(true)
                        && outcome
                            .result
                            .get("skipConversationRestore")
                            .and_then(JsonValue::as_bool)
                            == Some(true)
                    {
                        PluginEventOutcome::Generic(serde_json::json!({
                            "cancel": false,
                            "skipConversationRestore": true,
                        }))
                    } else {
                        PluginEventOutcome::from_wire(event_name, &outcome.result)
                    };
                    if event_name == "before_provider_request" && !outcome.result.is_null() {
                        if let Some(body) = event.payload.get_mut("payload") {
                            // `outcome.result` is not read after this point:
                            // `typed` above already owns its copy, and the
                            // remaining move (`outcome.errors`) is a different
                            // field, so the partial move is sound.
                            *body = outcome.result;
                        }
                    }
                    PluginEventOutcome::Js {
                        result: Box::new(typed),
                        errors: outcome.errors,
                    }
                }
                Err(error) => PluginEventOutcome::Error {
                    error: error.to_string(),
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Native dialog request/reply cell
// ---------------------------------------------------------------------------

/// First correlation id a NATIVE [`UiService`](crate::plugins::plugin::services::UiService)
/// dialog may use.
///
/// JS dialogs take `1, 2, 3, ...` from the runtime's own counter. Native ids
/// start in the top half of the `u64` space, so both faces put their requests
/// on the SAME [`crate::extensions::Notification::DialogRequest`] queue, are
/// promoted by the SAME FIFO overlay queue, and are answered by the SAME TUI
/// dialog responder without ever colliding. The id travels as the same `u64`
/// field and the same decimal-string reply key either face uses.
pub const NATIVE_DIALOG_ID_BASE: u64 = 1 << 63;

/// Typed request/reply half of a native `UiService::select/confirm/input/editor`
/// call.
///
/// The request itself does NOT live here: it is one ordinary
/// [`crate::extensions::Notification::DialogRequest`] on the JS runtime's
/// presentation queue (the exact kind/title/options wire shape the JS host fn
/// emits), so the TUI overlay, the FIFO `dialog_queue` promotion and the
/// timeout sweep need no native-specific queue. This cell owns only the
/// correlation half: the native id allocator and the one-shot reply sender the
/// TUI responder resolves. A cancelled or timed-out native future removes its
/// own entry ([`Self::cancel`]), so waiters never leak.
#[derive(Default)]
pub struct NativeDialogCell {
    /// Monotonic native sequence; ids are [`NATIVE_DIALOG_ID_BASE`] + this.
    next_id: AtomicU64,
    /// Pending native waiters keyed by dialog id.
    waiters: Mutex<std::collections::HashMap<u64, oneshot::Sender<JsonValue>>>,
}

impl NativeDialogCell {
    /// Whether `id` belongs to the reserved native range.
    pub fn is_native_dialog_id(id: u64) -> bool {
        id >= NATIVE_DIALOG_ID_BASE
    }

    /// Allocate a unique id and register its one-shot reply channel.
    ///
    /// The id is reserved before the caller pushes the notification, so a
    /// reply can never arrive for an unregistered id.
    pub fn register(&self) -> (u64, oneshot::Receiver<JsonValue>) {
        let id = NATIVE_DIALOG_ID_BASE.wrapping_add(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, sender);
        (id, receiver)
    }

    /// Deliver one dialog reply (the TUI's settle/submit/cancel/timeout
    /// branch). Returns `false` when the waiter is gone — a dropped or
    /// timed-out future — so the responder can stay silent instead of
    /// reviving it.
    pub fn respond(&self, id: u64, value: JsonValue) -> bool {
        let sender = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        sender.is_some_and(|sender| sender.send(value).is_ok())
    }

    /// Remove a pending waiter without resolving it (future dropped, cancelled
    /// or past its deadline).
    pub fn cancel(&self, id: u64) {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// Live waiter count (zero after every dialog settles, cancels or times
    /// out). Test/diagnostic probe for the no-leak contract.
    #[doc(hidden)]
    pub fn pending(&self) -> usize {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

// ---------------------------------------------------------------------------
// Raw terminal input (native `ctx.ui.onTerminalInput`)
// ---------------------------------------------------------------------------

/// Default per-handler budget for [`PluginHost::dispatch_terminal_input`].
///
/// The TUI imposes a shorter synchronous decision budget
/// (`tui::terminal_input::TERMINAL_INPUT_SYNC_BUDGET`); this bound protects the
/// host/worker side from a handler that never returns.
pub const DEFAULT_TERMINAL_INPUT_HANDLER_TIMEOUT: Duration = Duration::from_millis(50);

/// Bounds of the JS face's `terminal_input` budget. The JS prelude delivers
/// hooks through the actor mailbox, so an `await` in a handler (a dialog, a
/// small file read) easily outlives the native 50 ms budget; 2-5 s is the
/// async-handler window, capped so a hung handler cannot hold the adapter.
const TERMINAL_INPUT_JS_HANDLER_MIN: Duration = Duration::from_secs(2);
const TERMINAL_INPUT_JS_HANDLER_MAX: Duration = Duration::from_secs(5);

/// Native raw-terminal-input subscriptions (`UiService::on_terminal_input`).
///
/// The upstream JS face keeps its handlers in the prelude's registration
/// list; the native face keeps its own callbacks here, in registration order.
/// Both are consulted by [`PluginHost::dispatch_terminal_input`] for the same
/// physical input, so the two faces observe one event with identical
/// consume/replace semantics.
///
/// `interest` is the O(1) fast-path gate the TUI reads per key: a session
/// with no native registration skips the dispatch entirely (one atomic load),
/// exactly like the JS-side atomic gate (`ExtensionHandle::
/// has_terminal_input_handlers`).
#[derive(Default)]
pub struct TerminalInputRegistry {
    /// `(subscription id, handler)` in registration order.
    handlers: Mutex<Vec<(u64, Arc<dyn TerminalInputHandler>)>>,
    next_id: AtomicU64,
    /// `handlers` is non-empty. Kept as an atomic so the TUI's per-key fast
    /// path never takes the registry mutex.
    interest: AtomicBool,
}

impl TerminalInputRegistry {
    /// Register one native handler; the returned [`Subscription`] removes it
    /// (idempotently — cancelling twice is a no-op, matching upstream's
    /// unsubscribe function).
    pub fn register(self: &Arc<Self>, handler: Arc<dyn TerminalInputHandler>) -> Subscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, handler));
        self.interest.store(true, Ordering::Release);
        let registry = Arc::clone(self);
        Subscription::new(move || registry.remove(id))
    }

    fn remove(&self, id: u64) {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        handlers.retain(|(handler_id, _)| *handler_id != id);
        self.interest.store(!handlers.is_empty(), Ordering::Release);
    }

    /// Snapshot the handlers in registration order. A handler unsubscribed
    /// mid-dispatch still finishes this round (upstream iterates the live
    /// `Set`, but a snapshot makes the "one dispatch per face" contract
    /// observable and cannot recurse).
    fn snapshot(&self) -> Vec<Arc<dyn TerminalInputHandler>> {
        self.handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(_, handler)| Arc::clone(handler))
            .collect()
    }

    /// Whether any native subscription is live.
    pub fn has_handlers(&self) -> bool {
        self.interest.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// PluginHost
// ---------------------------------------------------------------------------

/// Precomputed event-interest table for the immutable plugin list.
///
/// [`AgentPlugin::events`] is a declaration made when the plugin joins the
/// host — the same immutability assumption [`PluginHost::build`] already
/// makes for `native_terminal_input`. Snapshotting it once here removes the
/// per-event `events()` call (a fresh `Vec<String>` for every `List` plugin)
/// and the per-plugin JS-adapter downcast from the fan-out paths.
///
/// Every list holds plugin indices in ascending (dispatch) order. Native
/// `All` plugins and the event-specific `List` plugins are separate sorted
/// lists so a lookup for an unlisted event costs one map probe.
#[derive(Default)]
struct NativeEventIndex {
    /// Non-JS plugins with [`EventInterest::All`].
    all_native: Vec<usize>,
    /// Non-JS plugins with a `List` interest, keyed by event name.
    by_event: HashMap<String, Vec<usize>>,
    /// The JS adapter positions (each receives every event).
    js_adapters: Vec<usize>,
}

impl NativeEventIndex {
    fn build(plugins: &[Arc<dyn AgentPlugin>]) -> Self {
        let mut index = Self::default();
        for (position, plugin) in plugins.iter().enumerate() {
            if is_js_adapter(plugin) {
                index.js_adapters.push(position);
                continue;
            }
            match plugin.events() {
                EventInterest::None => {}
                EventInterest::All => index.all_native.push(position),
                EventInterest::List(mut names) => {
                    // A duplicated name in one declaration must still admit
                    // the plugin exactly once per event (`matches` semantics).
                    names.sort();
                    names.dedup();
                    for name in names {
                        index.by_event.entry(name).or_default().push(position);
                    }
                }
            }
        }
        index
    }

    fn has_native_handlers(&self, event: &str) -> bool {
        !self.all_native.is_empty()
            || self
                .by_event
                .get(event)
                .is_some_and(|positions| !positions.is_empty())
    }

    /// Plugin indices to invoke for `event`, in dispatch order. `include_js`
    /// merges the JS adapter positions (which are handed every event, exactly
    /// like [`EventInterest::All`] native plugins).
    fn dispatch_indices(&self, event: &str, include_js: bool) -> DispatchIndices<'_> {
        DispatchIndices {
            lists: [
                self.all_native.as_slice(),
                self.by_event.get(event).map_or(&[], Vec::as_slice),
                if include_js {
                    self.js_adapters.as_slice()
                } else {
                    &[]
                },
            ],
            positions: [0; 3],
            last: None,
        }
    }
}

/// Three-way merge of the sorted index lists in [`NativeEventIndex`],
/// yielding each plugin index once in ascending order.
struct DispatchIndices<'a> {
    lists: [&'a [usize]; 3],
    positions: [usize; 3],
    last: Option<usize>,
}

impl Iterator for DispatchIndices<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            let mut best: Option<(usize, usize)> = None;
            for (list, position) in self.positions.iter().enumerate() {
                if let Some(&value) = self.lists[list].get(*position) {
                    if best.is_none_or(|(current, _)| value < current) {
                        best = Some((value, list));
                    }
                }
            }
            let (value, list) = best?;
            self.positions[list] += 1;
            // Defensive: an index can only appear in one list today (the
            // variants are exclusive, and the adapter is excluded from both
            // native lists), but the merge contract stays a set either way.
            if self.last == Some(value) {
                continue;
            }
            self.last = Some(value);
            return Some(value);
        }
    }
}

/// Unified plugin host: ordered plugin list + shared registry + event
/// dispatch.
pub struct PluginHost {
    plugins: Vec<Arc<dyn AgentPlugin>>,
    /// Cached per-event dispatch table over the immutable `plugins` list.
    native_events: NativeEventIndex,
    /// The JS runtime (also present in `plugins` through the adapter).
    js: Option<ExtensionHandle>,
    /// Whether this host owns the JS runtime lifecycle. `false` only for a
    /// host built around another session's handle (the subagent child's
    /// shared-handle fallback): its shutdown runs the native hooks but must
    /// not terminate the parent's actor.
    js_owned: AtomicBool,
    /// The native dialog request/reply cell shared with the TUI responder.
    native_dialogs: Arc<NativeDialogCell>,
    /// Outermost-UI-prompt nesting state shared by every native dialog on
    /// this host (upstream `withUIPrompt`'s `uiPromptDepth` +
    /// `activeUIPrompt`, runner.ts:589-621): concurrent dialogs emit ONE
    /// `ui_prompt_start` / `ui_prompt_end` pair, for the outermost prompt's
    /// kind/title. The JS face keeps its own prelude counter (its events are
    /// delivered to JS handlers only).
    ui_prompt: Mutex<(usize, Option<(String, String)>)>,
    /// Native `ctx.ui.onTerminalInput` subscriptions (raw terminal input).
    terminal_input: Arc<TerminalInputRegistry>,
    /// Whether a non-JS plugin declared `terminal_input` interest at build
    /// time. The plugin list is immutable after [`PluginHost::build`], so this
    /// is a plain `bool`: the TUI's per-key fast path must not walk plugins.
    native_terminal_input: bool,
    registry: RwLock<Arc<PluginRegistry>>,
    /// Monotonic count of successful registry rebuilds: incremented every
    /// time [`PluginHost::rebuild_registry_with`] publishes a new snapshot.
    ///
    /// This is the definition fingerprint the session's merged-registry cache
    /// keys on. A rebuild can keep every tool name while a description,
    /// parameter schema, output schema or exposure changes (the MCP dirty
    /// path and JS re-registrations both do), so a name-keyed cache would
    /// keep handing out stale [`crate::agent::tool_execution::AgentTool`]
    /// objects. `Release`/`Acquire` pairs with the snapshot read so a cache
    /// that observes a new generation also observes that rebuild's registry.
    registry_generation: AtomicU64,
    cwd: PathBuf,
    /// Last JS-runtime definition snapshot (skip no-op rebuilds). The JS
    /// adapter is the only plugin whose registered definitions move at runtime
    /// — every builtin re-registers the same definitions on each pass — so the
    /// change check compares against THIS, never a full registry snapshot (a
    /// full snapshot can never equal the JS-only defs: the builtins always
    /// contribute their own). One fingerprint per definition class
    /// [`JsExtensionsPlugin::register`] imports; see [`JsDefFingerprints`].
    js_defs: RwLock<JsDefSnapshot>,
    /// Optional O(1) definition-generation probe: when installed, the gate
    /// skips the deep fingerprint read while the generation is unchanged.
    /// Installed at build time when the runtime can answer cheaply (the
    /// contract lives on [`JsDefGenerationProbe`]); `None` keeps the exact
    /// deep read as the only gate, byte for byte the pre-probe behavior.
    js_def_gen: Option<JsDefGenerationProbe>,
    /// How many deep fingerprint reads ([`js_def_fingerprints`]) this host ran
    /// — the per-event work [`JsDefGenerationProbe`] removes. Test probe
    /// ([`PluginHost::js_def_fingerprint_reads`]); per-host (never a global) so
    /// parallel tests in one binary cannot observe each other's reads.
    js_def_reads: AtomicU64,
    /// The `mcp` builtin's shared manager, when the plugin is loaded. The
    /// refresh path polls its dirty flags (discovered tools, JS server
    /// registrations) to rebuild the registry. Without the `mcp` feature the
    /// handle type is a never-constructed placeholder, so the field stays
    /// `None` and every read is a no-op.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    mcp_manager: Option<McpManagerHandle>,
    /// Position of the surviving `mcp` builtin in `plugins`. The refresh tail
    /// consults the plugin on every emit; a cached position keeps that an
    /// O(1) lookup instead of a downcast scan of the whole list.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    mcp_plugin_index: Option<usize>,
    /// One-shot guard for [`PluginHost::shutdown`]: the first call runs every
    /// plugin's `on_shutdown`; later calls (session teardown racing a child
    /// run's own teardown) are no-ops.
    shut_down: AtomicBool,
    /// The service context bound by [`PluginHost::bind_context`], when any.
    /// ONE context per host, replaced wholesale on rebind; plugins receive
    /// identity-scoped clones through `on_context`.
    context: RwLock<Option<PluginContext>>,
    /// The last [`SessionContextSnapshot`] pushed through
    /// [`PluginHost::update_session_context`], retained so native plugins and
    /// the live session service read the exact payload instead of rebuilding
    /// or re-serializing it. Replaced wholesale on every push; an `Arc`
    /// cloned before a push keeps observing the old snapshot.
    session_context: RwLock<Option<Arc<SessionContextSnapshot>>>,
    /// The `entries` field of [`PluginHost::session_context`] as its own
    /// `Arc` (one clone per push), backing
    /// `SessionService::entries` without a deep JSON clone per read.
    session_entries: RwLock<Option<Arc<JsonValue>>>,
    /// Serializes [`PluginHost::bind_context`]'s epoch bump + store + notify
    /// sequence. Without it two concurrent binds can interleave (A bumps 1,
    /// B bumps 2 and stores, A stores last) leaving an epoch-1 context under a
    /// live epoch of 2, and can deliver `on_context` out of epoch order.
    ///
    /// Not reentrant: binding from inside `on_context` deadlocks by design
    /// (hosts bind from their own lifecycle paths, never from a plugin
    /// callback).
    bind_lock: Mutex<()>,
    /// The live generation cell shared by every context this host binds.
    ///
    /// `bind_context` increments it before stamping the new context, so all
    /// previously bound contexts (and clones plugins cached) observe a newer
    /// live epoch and report themselves stale. This is the `/reload`
    /// invalidation rule in executable form.
    context_epoch: Arc<AtomicU64>,
    /// Paths whose registration FAILED, in first-failure order and deduped.
    /// A plugin whose registration errored is not in the registry at all
    /// (upstream load-error isolation), so this is what lets a host answer
    /// "did the plugin I asked for actually attach?" instead of treating the
    /// silent absence as success.
    ///
    /// Lifecycle (invariant): **a failure never disappears without its plugin
    /// also leaving the configuration.** [`Self::build`] removes every failed
    /// plugin from the DISPATCH list, and a rebuild re-runs that list only, so
    /// a build-time failure has no later pass that could re-observe it —
    /// replacing the ledger on a rebuild would therefore silently launder it
    /// into "clean" (the `std::mem::take`-class defect).
    /// [`Self::rebuild_registry_with`] instead SEEDS the pass with the entries
    /// of paths it does not re-run ([`Self::standing_registration_failures`])
    /// and lets the pass re-derive the ledger, so an entry stands unless a pass
    /// that actually re-ran its path observes success. Configuration is
    /// immutable after build, so "the plugin is no longer configured" is only
    /// ever observed as a NEW [`Self::build`], whose ledger starts clean.
    registration_failures: RwLock<Vec<RegistrationFailure>>,
    /// One-shot guard for [`PluginHost::run_init_hooks`]: `on_init` runs once
    /// per host, before the first dispatched event.
    init_hooks_run: AtomicBool,
}

/// Why one plugin's `register()` pass did not produce a registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationFailure {
    /// The plugin's display path (`builtin:<name>`, a file path, `<inline:...>`).
    pub path: String,
    /// The registration error message.
    pub message: String,
}

/// Outcome of [`PluginHost::call_tool_with_context`]: the JS-wire result plus
/// the native tool's images as structured values (the wire result also
/// carries them as `{type:"image"}` content items).
#[derive(Debug, Clone)]
pub struct ToolCallOutcome {
    /// Upstream `AgentToolResult` wire shape.
    pub result: crate::extensions::ToolResult,
    /// Image blocks the tool produced, mirrored out of `result.content` (a JS
    /// tool's images exist only there; a native tool carries them as the
    /// structured `ToolResult.images`).
    pub images: Vec<crate::ai::types::ImageContent>,
}

/// The committed JS-definition snapshot: the fingerprints the host last
/// compared/published plus the runtime generation they were read at.
#[derive(Default)]
struct JsDefSnapshot {
    fingerprints: JsDefFingerprints,
    /// The [`JsDefGenerationProbe`] value read BEFORE `fingerprints` (the same
    /// read-the-generation-first protocol `ExtensionRuntime::refresh_registry`
    /// uses for its own snapshot): a definition write landing between the two
    /// reads leaves the live generation newer than this, so the next check
    /// re-reads instead of trusting the snapshot. `None` = no probe installed
    /// (or a fresh probe install), which forces the next check to re-read.
    generation: Option<u64>,
}

/// O(1) probe answering "have the JS runtime's definition registrations moved
/// since generation G?".
///
/// The host gates its registry rebuild on it: an unchanged generation PROVES
/// no definition write happened since the committed snapshot was read, while a
/// moved generation only means "re-read the defs and compare" (the exact
/// [`JsDefFingerprints`] comparison stays the rebuild decision, so a spurious
/// bump costs one read, never a rebuild).
///
/// Contract for whatever runtime-side generation is installed here: it must
/// move on EVERY write to a definition class [`JsExtensionsPlugin::register`]
/// imports. The embedded runtime answers it with `RuntimeState::defs_gen`,
/// exposed as [`ExtensionHandle::defs_gen`] and bumped by every one of those
/// write sites — the JS registry's `registryDirty` signal (tools, commands,
/// shortcuts; `registry_gen` alone is not enough there, because the host must
/// learn about the write without forcing the runtime's expensive
/// `__pir_exportRegistryJson`) and the host fns writing flags, providers, MCP
/// servers and virtual models directly, which never touch the export at all.
/// [`PluginHost::build`] installs that runtime probe automatically whenever it
/// was given a JS handle; a host built without one keeps `None` and the exact
/// deep read. See the module note in [`PluginHost::refresh_registry_if_changed`].
pub type JsDefGenerationProbe = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The gate's O(1) half ([`PluginHost::js_def_gate`]).
enum DefGate {
    /// The committed snapshot is provably current: skip the deep read.
    Clean,
    /// Read the defs; the carried generation was observed before this decision
    /// (so a write racing the read stays on the "moved" side of the next
    /// check). `None` = no probe: the deep read's result carries no generation.
    Read(Option<u64>),
}

/// Fingerprints of the JS runtime's registry snapshot — one fingerprint per
/// definition class [`JsExtensionsPlugin::register`] imports. The host
/// rebuilds the shared registry when any of these moves
/// ([`PluginHost::refresh_registry_if_changed`]), so a JS handler that
/// registers a provider, a virtual model, a command, a flag, a shortcut or an
/// MCP server WITHOUT touching its tool names still reaches the registry.
///
/// Every fingerprint is an array of small JSON strings, computed from the
/// runtime's exported defs and compared element-wise; no hashing and no
/// serialization of the whole registry. Classes whose consumers key their
/// entries by a name/id (flags, shortcuts, providers, MCP servers, virtual
/// models) are sorted before comparison, because the runtime's registrations
/// upsert (retain-then-push) and can therefore reorder an unchanged set — a
/// pure reorder carries no registry change and must not force a rebuild.
#[derive(Default, PartialEq, Eq)]
struct JsDefFingerprints {
    /// Sorted JS tool NAMES. Sound as a change signal because the JS registry
    /// keeps the FIRST registration per name (`state.toolMeta[name] ===
    /// undefined` in the prelude's `registerTool`): a same-name
    /// re-registration cannot alter the exported definition's fields.
    tools: Vec<String>,
    /// One `{name, description}` JSON per exported command, in export ORDER —
    /// order is load-bearing here: duplicate names resolve to `name:2`,
    /// `name:3`, ... by occurrence (`PluginRegistry::resolved_commands`), so a
    /// same-name re-registration is a real registry change and must not be
    /// collapsed by sorting. Exactly the fields the registration pass
    /// consumes.
    commands: Vec<String>,
    /// One `{name, type, description, default}` JSON per registered flag,
    /// sorted by name.
    flags: Vec<String>,
    /// One `{key, description}` JSON per registered shortcut, sorted by key.
    shortcuts: Vec<String>,
    /// One `{id, config}` JSON per `pi.registerProvider` definition, sorted by
    /// id (the runtime upserts by id, so at most one entry per id).
    providers: Vec<String>,
    /// One `{name, config, extensionPath}` JSON per `pi.registerMcpServer`
    /// definition, sorted by name. The config is part of the fingerprint: the
    /// `mcp` builtin re-registers a same-name server whose config differs
    /// (`apply_js_server_defs` compares `existing.config == config`) and
    /// publishes the new config in the `mcp_servers_change` payload, so
    /// name-only would silently drop a config change.
    mcp_servers: Vec<String>,
    /// One full virtual-model definition JSON per `pi.registerVirtualModel`,
    /// sorted by definition text (the runtime upserts by (provider, id)).
    virtual_models: Vec<String>,
}

/// Upstream `SessionShutdownEvent` payload (`extensions/types.ts:824-831`):
/// `{type, reason, targetSessionFile?}`. The destination file is OMITTED when
/// the teardown has no replacement target (quit) — never emitted as null.
fn session_shutdown_payload(reason: &str, target_session_file: Option<&str>) -> JsonValue {
    let mut payload = serde_json::json!({
        "type": "session_shutdown",
        "reason": reason,
    });
    if let Some(target) = target_session_file {
        payload["targetSessionFile"] = JsonValue::String(target.to_owned());
    }
    payload
}

/// Run one native plugin's event handler with panic isolation.
///
/// A Rust builtin/inline plugin shares the caller's task; a panic in its
/// future would unwind through the whole agent loop (or the TUI event
/// handler). The plugin boundary is an isolation boundary here: a panic is
/// converted to the same [`PluginEventOutcome::Error`] a returned error takes,
/// so the merge layer reports it and the remaining plugins still run in
/// order. The JS adapter is never routed through this helper (its own actor
/// already isolates engine panics).
async fn dispatch_native_event(
    plugin: &Arc<dyn AgentPlugin>,
    event: &str,
    payload: &mut JsonValue,
) -> PluginEventOutcome {
    let future = std::panic::AssertUnwindSafe(plugin.on_event(PluginEvent::new(event, payload)));
    match future.catch_unwind().await {
        Ok(outcome) => outcome,
        Err(panic) => PluginEventOutcome::Error {
            error: format!(
                "plugin `{}` panicked: {}",
                plugin.info().name,
                panic_text(panic)
            ),
        },
    }
}

/// Best-effort text of a caught panic payload (`&str` / `String`).
fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

impl PluginHost {
    /// Build the host around an optional JS runtime and a list of inline
    /// Rust plugins (appended last, upstream `<inline:...>` factories).
    ///
    /// The plugin order follows the JS runtime's recorded path plan: CLI
    /// `-e` paths first (including `builtin:<name>` entries resolved here),
    /// then discovered default paths, with the JS adapter inserted at the
    /// position of its first file path — mirroring upstream's
    /// `extensionFactories = [...builtInExtensions, ...]` wiring combined
    /// with path-ordered loading.
    pub fn build(js: Option<ExtensionHandle>, inline: Vec<Arc<dyn AgentPlugin>>) -> Self {
        let build_started = crate::config::timings::start_phase();
        let mut diagnostics: Vec<PluginDiagnostic> = Vec::new();
        let cwd = js
            .as_ref()
            .map(|handle| handle.config().cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let mut plugins: Vec<Arc<dyn AgentPlugin>> = Vec::new();
        let mut js_inserted = false;
        if let Some(handle) = &js {
            // Native-first `tool-repair` precedence: a `builtin:tool-repair`
            // entry is ALWAYS instantiated, and coexistence with the JS
            // `pi-tool-repair` package resolves to the native face alone (the
            // loop applies the generic JS `message_end` return value since the
            // parity lane, so the rule is no longer "the JS repair would be
            // dropped" — it is "the same repair pipeline must not run
            // twice"). Coexistence is decided where the path plan becomes
            // the runtime load list, in the loader
            // ([`super::loader::tool_repair_precedence`]): with the builtin
            // on the effective plan the JS `pi-tool-repair` entry is dropped
            // from BOTH the plan and the load list, so the JS face (its
            // `message_end` repair and Phase 0 alike) never runs; with the
            // builtin disabled the JS entry stays — the fallback face, so the
            // user never loses both.
            //
            // The old rule here did the opposite (JS package on the plan ⇒
            // skip the builtin) to avoid a native-vs-JS Phase 0 double-run.
            // That rationale is void twice over. (a) A double-run would be
            // harmless for Phase 0: the native Phase 0 is idempotent — the
            // one mutating rule strips EVERY `^`/`$` char from a schema
            // `pattern` (a second strip removes nothing) or deletes a pattern
            // that mixes `|` with anchors (a second pass re-walks it as an
            // absent key); the schema walk never adds a `pattern` key, so a
            // second `repair::sanitize_request_payload` reports
            // `changed == false` and leaves the body byte-identical. (b)
            // Suppressing the builtin is the costly direction: while the JS
            // `message_end` return was dropped it took the whole repair
            // pipeline down with it. Swapping to the native port costs the
            // user nothing — it is a faithful port of the same package, and
            // the deviations its module head
            // (`src/plugins/tool_repair.rs`) documents are net gains (real
            // core-tool schemas) or representation limits, not lost
            // behavior.
            for path in handle.path_plan() {
                let text = path.to_string_lossy();
                if let Some(name) = builtin_name(&text) {
                    match instantiate_builtin(name) {
                        Some(plugin) => {
                            // The `mcp` builtin re-roots at the SESSION cwd
                            // (runtime config; a child session passes its task
                            // cwd): `.pi/mcp.json` discovery and stdio spawns
                            // follow the session's working directory (upstream
                            // per-session cwd), never the process cwd. The
                            // factory-default instance is discarded unused.
                            #[cfg(feature = "mcp")]
                            let plugin: Arc<dyn AgentPlugin> = if plugin
                                .as_any()
                                .and_then(|any| any.downcast_ref::<McpPlugin>())
                                .is_some()
                            {
                                Arc::new(McpPlugin::with_cwd(
                                    &crate::coding_agent::context::agent_dir(),
                                    &cwd,
                                ))
                            } else {
                                plugin
                            };
                            plugins.push(plugin);
                        }
                        None => {
                            // The message reads `name`, which borrows `text`: build it
                            // before the owned path takes the string.
                            let message = super::builtin::missing_builtin_message(name);
                            let path = text.into_owned();
                            diagnostics.push(PluginDiagnostic {
                                kind: super::registry::DiagnosticKind::Error,
                                path: path.clone(),
                                message,
                            });
                        }
                    }
                } else if !js_inserted {
                    plugins.push(Arc::new(JsExtensionsPlugin::new(handle.clone())));
                    js_inserted = true;
                }
            }
            if !js_inserted {
                // JS runtime loaded without a recorded plan (direct handle
                // start): the adapter takes the first dispatch slot.
                plugins.push(Arc::new(JsExtensionsPlugin::new(handle.clone())));
            }
        }
        plugins.extend(inline);

        // Attach the JS runtime handle to the `mcp` builtin BEFORE
        // registration, so its `register()` sees `pi.registerMcpServer`
        // definitions and can dispatch `mcp_servers_change` to extensions.
        // The `subagent` builtin gets the same seam: its tool passes the
        // handle into child sessions so children own the extension tool
        // surface (upstream's child is a full `pi`).
        //
        // `mcp` and `subagent` are the only handle-consuming seams: in a
        // build with neither feature the walk is compiled out.
        #[cfg(any(feature = "mcp", feature = "subagent"))]
        for plugin in &plugins {
            #[cfg(feature = "mcp")]
            if let Some(mcp) = plugin
                .as_any()
                .and_then(|any| any.downcast_ref::<McpPlugin>())
            {
                mcp.attach_js(js.clone());
            }
            #[cfg(feature = "subagent")]
            if let Some(subagent) = plugin
                .as_any()
                .and_then(|any| any.downcast_ref::<super::subagent::SubagentPlugin>())
            {
                subagent.attach_js(js.clone());
            }
        }

        // Unified runtime-attach hook (the plugin-side mirror of upstream's
        // factory receiving the extension API): every plugin — not just the
        // two named seams above — learns whether this session has a JS runtime
        // and, when it does, receives the handle BEFORE the registration pass,
        // so `register()` can build handlers that already know the host's UI
        // posture (`ctx.hasUI`; the `todo` builtin's `/todos` gate).
        for plugin in &plugins {
            plugin.on_runtime_attached(js.clone());
        }

        let (registry, registration_failures) = build_registry(&plugins, &mut diagnostics);
        // The dispatch list keeps only the registry survivors: plugins that
        // failed to load or were omitted as replaceable (upstream
        // omitReplacedExtensions) never run. Borrowed paths: no per-plugin
        // `String` clone at startup.
        let kept: std::collections::HashSet<&str> = registry
            .plugins()
            .iter()
            .map(|plugin| plugin.info.path.as_str())
            .collect();
        plugins.retain(|plugin| kept.contains(plugin.info().path.as_str()));

        // Only a SURVIVING mcp builtin owns the host's manager: a replaced
        // one is omitted entirely and must not stay wired in.
        #[cfg(feature = "mcp")]
        let mcp_manager = plugins.iter().find_map(|plugin| {
            plugin
                .as_any()
                .and_then(|any| any.downcast_ref::<McpPlugin>())
                .map(|mcp| mcp.manager().clone())
        });
        #[cfg(not(feature = "mcp"))]
        let mcp_manager: Option<McpManagerHandle> = None;
        #[cfg(feature = "mcp")]
        let mcp_plugin_index = plugins
            .iter()
            .position(|plugin| plugin.as_any().is_some_and(|any| any.is::<McpPlugin>()));
        #[cfg(not(feature = "mcp"))]
        let mcp_plugin_index: Option<usize> = None;
        // The definition snapshot the build's registration pass just consumed.
        // It is committed WITHOUT a generation (the probe below is installed
        // only after it), so the first gate check re-reads the defs and latches
        // a generation from there.
        let (js_defs, js_def_reads) = match js.as_ref() {
            Some(js) => (
                JsDefSnapshot {
                    fingerprints: js_def_fingerprints(js),
                    generation: None,
                },
                1,
            ),
            None => (JsDefSnapshot::default(), 0),
        };

        // The runtime-side O(1) definition generation, read across the actor's
        // shared state ([`ExtensionHandle::defs_gen`]) — the closure is built
        // here, BEFORE `js` moves into the struct below.
        //
        // Invariant with the probe installed: a definition write through ANY
        // runtime path — the JS-registry classes (tools/commands/shortcuts,
        // whose `registryDirty` bump covers both the JS registration and the
        // export commit that publishes it) or the host fns writing flags,
        // providers, MCP servers and virtual models directly — moves this
        // generation, so the next emit re-reads the fingerprints and a real
        // change is published. `None` (no runtime to ask) leaves the gate
        // byte for byte the exact deep-read fallback it was before the probe.
        let js_def_gen: Option<JsDefGenerationProbe> = match js.clone() {
            Some(handle) => {
                let probe: JsDefGenerationProbe = Arc::new(move || handle.defs_gen());
                Some(probe)
            }
            None => None,
        };

        // Snapshot the event interests ONCE: the plugin list never changes
        // after build (the same invariant `native_terminal_input` documents),
        // so every later fan-out can use the cached table instead of asking
        // each plugin `events()` per event.
        let native_events = NativeEventIndex::build(&plugins);
        // Whether a non-JS plugin declared `terminal_input` interest at build
        // time — read from the same table the dispatch uses.
        let native_terminal_input = native_events.has_native_handlers("terminal_input");

        let host = Self {
            plugins,
            native_events,
            js,
            js_owned: AtomicBool::new(true),
            native_dialogs: Arc::new(NativeDialogCell::default()),
            ui_prompt: Mutex::new((0, None)),
            terminal_input: Arc::new(TerminalInputRegistry::default()),
            native_terminal_input,
            registry: RwLock::new(Arc::new(registry)),
            registry_generation: AtomicU64::new(0),
            cwd,
            js_defs: RwLock::new(js_defs),
            js_def_gen,
            js_def_reads: AtomicU64::new(js_def_reads),
            mcp_manager,
            mcp_plugin_index,
            shut_down: AtomicBool::new(false),
            context: RwLock::new(None),
            session_context: RwLock::new(None),
            session_entries: RwLock::new(None),
            bind_lock: Mutex::new(()),
            context_epoch: Arc::new(AtomicU64::new(0)),
            registration_failures: RwLock::new(registration_failures),
            init_hooks_run: AtomicBool::new(false),
        };
        // Builtin instantiation + the one registration pass over every plugin.
        // `--only-builtin` still pays this (native factories), but never the
        // QuickJS start/load phases.
        crate::config::timings::log_phase_once_opt(
            "plugins.host.build",
            "plugins.host.build",
            crate::config::timings::finish_phase(build_started),
        );
        host
    }

    /// The ordered plugin list.
    pub fn plugins(&self) -> &[Arc<dyn AgentPlugin>] {
        &self.plugins
    }

    /// The native dialog request/reply cell.
    ///
    /// The TUI dialog responder routes a native id through
    /// [`NativeDialogCell::respond`] instead of
    /// [`ExtensionHandle::respond_dialog`], so a native
    /// `UiService::select/confirm/input/editor` future resolves from the exact
    /// same overlay submit/Esc/timeout path a JS dialog uses. The cell is
    /// shared with the live `UiService` binding for this host.
    pub fn native_dialogs(&self) -> Arc<NativeDialogCell> {
        Arc::clone(&self.native_dialogs)
    }

    /// Register a native raw-terminal-input handler (upstream
    /// `ctx.ui.onTerminalInput`, the native face).
    ///
    /// Handlers run in registration order before the built-in TUI input
    /// handling (and before the JS face's handlers, which ride the adapter at
    /// their ordered plugin-list position). Cancelling the returned
    /// [`Subscription`] removes the handler; sessions with no subscription
    /// skip the whole dispatch ([`Self::has_terminal_input_handlers`]).
    pub fn on_terminal_input(&self, handler: Arc<dyn TerminalInputHandler>) -> Subscription {
        self.terminal_input.register(handler)
    }

    /// Whether any face registered a raw-terminal-input handler.
    ///
    /// O(1) and lock-free: two atomic loads plus the build-time native
    /// interest flag. The TUI checks this per physical input before paying a
    /// dispatch-thread round trip, so sessions without handlers keep the exact
    /// pre-hook input path.
    pub fn has_terminal_input_handlers(&self) -> bool {
        self.terminal_input.has_handlers()
            || self.native_terminal_input
            || self
                .js
                .as_ref()
                .is_some_and(ExtensionHandle::has_terminal_input_handlers)
    }

    /// Dispatch one raw terminal input chunk to both faces.
    ///
    /// Upstream `handleTerminalInput` semantics, with the pir face order
    /// documented on [`Self::dispatch_terminal_input`]:
    ///
    /// 1. Native `UiService::on_terminal_input` handlers, in registration
    ///    order.
    /// 2. Native `AgentPlugin::on_event("terminal_input")` handlers, in the
    ///    host's ordered plugin list.
    /// 3. The JS adapter (its `ctx.ui.onTerminalInput` handlers, in
    ///    registration order), skipped entirely when the runtime reports no
    ///    terminal-input handler.
    ///
    /// A handler's `{consume: true}` stops the walk; a `{data}` replacement is
    /// written back into the payload, so later handlers and the built-in path
    /// observe the replaced input. A replaced-to-empty chunk is consumed
    /// (upstream: `if (current.length === 0) return;`). Handler errors are
    /// isolated and fall through: they never consume the input.
    pub async fn dispatch_terminal_input(&self, data: &str) -> TerminalInputAction {
        self.dispatch_terminal_input_with_timeout(data, DEFAULT_TERMINAL_INPUT_HANDLER_TIMEOUT)
            .await
    }

    /// [`Self::dispatch_terminal_input`] with an explicit per-handler budget.
    ///
    /// The JS adapter goes through `ExtensionHandle::emit_with_timeout`, so a
    /// handler that misses the budget is marked ABANDONED by the runtime (its
    /// late side effects are dropped, not applied to a later run). Each native
    /// plugin future is bounded the same way; a missed deadline falls through.
    /// The JS face's budget for one `terminal_input` delivery: the extension
    /// call timeout (clamped to 2-5 s), never the native short budget. A
    /// native plugin's synchronous callback keeps `native_budget`.
    fn js_terminal_input_timeout(&self, native_budget: Duration) -> Duration {
        let configured = self
            .js
            .as_ref()
            .map(|js| js.call_timeout())
            .unwrap_or(native_budget);
        configured
            .clamp(TERMINAL_INPUT_JS_HANDLER_MIN, TERMINAL_INPUT_JS_HANDLER_MAX)
            .max(native_budget)
    }

    pub async fn dispatch_terminal_input_with_timeout(
        &self,
        data: &str,
        handler_timeout: Duration,
    ) -> TerminalInputAction {
        // Cow, not `to_owned()`: this runs for every terminal input chunk
        // (keystrokes and pastes) and the overwhelmingly common case is that
        // no handler rewrites the data. Borrowing the caller's chunk keeps
        // the walk copy-free; only an actual `Replace` promotes it to an
        // owned buffer.
        let mut current: std::borrow::Cow<'_, str> = std::borrow::Cow::Borrowed(data);
        // Only the native faces keep the short synchronous budget; the JS
        // face runs on the actor mailbox where a legitimate async handler
        // can take far longer than a typing frame.
        let js_timeout = self.js_terminal_input_timeout(handler_timeout);
        for handler in self.terminal_input.snapshot() {
            match handler.handle(&current) {
                TerminalInputAction::Pass => {}
                TerminalInputAction::Consume => return TerminalInputAction::Consume,
                TerminalInputAction::Replace(next) => current = std::borrow::Cow::Owned(next),
            }
        }

        let mut payload = serde_json::json!({ "data": current });
        // Face order is native → JS, independent of the adapter's position in
        // the ordered plugin list (the path plan may put it first). A native
        // consume therefore short-circuits before the JS face runs, matching
        // upstream's single listener list rule for later listeners.
        for index in self.native_events.dispatch_indices("terminal_input", false) {
            let plugin = &self.plugins[index];
            let outcome = match tokio::time::timeout(
                handler_timeout,
                plugin.on_event(PluginEvent::new("terminal_input", &mut payload)),
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(_) => {
                    crate::ext_diag!(
                        "[ext] terminal_input handler timed out in {}",
                        plugin.info().path
                    );
                    continue;
                }
            };
            match outcome {
                PluginEventOutcome::Error { error } => {
                    // Fail open: the built-in handling receives the input.
                    crate::ext_diag!("[ext] terminal_input dispatch failed: {error}");
                }
                PluginEventOutcome::TerminalInput { consume, data } => {
                    if let Some(next) = data {
                        payload["data"] = JsonValue::String(next);
                    }
                    if consume {
                        return TerminalInputAction::Consume;
                    }
                }
                _ => {}
            }
        }

        // Exactly one JS delivery for the face. A runtime with no
        // terminal-input handler must not pay a mailbox round trip.
        if let Some(js) = &self.js {
            if js.has_terminal_input_handlers() {
                match js
                    .emit_with_timeout("terminal_input", &mut payload, js_timeout)
                    .await
                {
                    Ok(outcome) => {
                        for error in outcome.errors {
                            crate::ext_diag!(
                                "[ext] terminal_input handler failed: {}",
                                error.error
                            );
                        }
                        if let PluginEventOutcome::TerminalInput { consume, data } =
                            PluginEventOutcome::from_wire("terminal_input", &outcome.result)
                        {
                            if let Some(next) = data {
                                payload["data"] = JsonValue::String(next);
                            }
                            if consume {
                                return TerminalInputAction::Consume;
                            }
                        }
                    }
                    Err(error) => {
                        // Fail open: the built-in handling receives the input.
                        crate::ext_diag!("[ext] terminal_input JS dispatch failed: {error}");
                    }
                }
            }
        }

        let final_data = payload
            .get("data")
            .and_then(JsonValue::as_str)
            .unwrap_or(data);
        if final_data.is_empty() && !data.is_empty() {
            TerminalInputAction::Consume
        } else if final_data != data {
            TerminalInputAction::Replace(final_data.to_owned())
        } else {
            TerminalInputAction::Pass
        }
    }

    /// Bind (or rebind) the typed host-service context for this host.
    ///
    /// This is the single service handoff and the single invalidation point:
    ///
    /// 1. The host's live generation cell is incremented.
    /// 2. The incoming context is re-stamped with that generation and the
    ///    host-owned cell, then stored.
    /// 3. Every plugin receives an identity-scoped clone through
    ///    [`AgentPlugin::on_context`] exactly once for this binding.
    ///
    /// A context bound by an earlier call — or a clone a plugin cached from
    /// it — still points at the same generation cell, so it now answers
    /// [`PluginContext::is_active`] `false` /
    /// [`PluginContext::check_active`] `Err(Stale)`. Hosts must call this
    /// again after `/reload`, `newSession`, `fork`, `switchSession`, or any
    /// session replacement; there is no other epoch bookkeeping.
    ///
    /// Binding is additive: calling this is the only way `on_context` fires,
    /// so a host (and every existing plugin test) that never binds keeps the
    /// pre-service behavior byte for byte.
    ///
    /// Concurrency: the whole bump → store → notify sequence is serialized by
    /// the host's bind mutex, so concurrent binds cannot leave the stored
    /// context behind the live epoch or deliver `on_context` out of order. A
    /// concurrent reader of [`Self::context`] observes either the previous
    /// complete context or the new complete context. Binding is not reentrant:
    /// calling `bind_context` from inside `on_context` deadlocks.
    pub fn bind_context(&self, ctx: PluginContext) {
        let _binding = self
            .bind_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `AcqRel` pairs with the `Acquire` load in `PluginContext::is_active`:
        // a plugin that observes the new epoch also observes every write the
        // rebinding host made before it.
        let epoch = self
            .context_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let bound = ctx.with_epoch(Arc::clone(&self.context_epoch), epoch);
        // Store before notifying: an `on_context` implementation may consult
        // `host.context()` (e.g. to hand the context to a worker) and must
        // observe the new binding.
        *self
            .context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bound.clone());
        for (order, plugin) in self.plugins.iter().enumerate() {
            let plugin_ctx = bound.with_identity(PluginIdentity::from_plugin(plugin.info(), order));
            plugin.on_context(&plugin_ctx);
        }
    }

    /// The context currently bound through [`Self::bind_context`], if any.
    pub fn context(&self) -> Option<PluginContext> {
        self.context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The host's current generation (the value the next `bind_context`
    /// consumes + 1). Diagnostic/perf seam.
    #[doc(hidden)]
    pub fn context_epoch(&self) -> u64 {
        self.context_epoch.load(Ordering::Acquire)
    }

    /// Native status-line entries across the ordered plugin list, as
    /// `(key, text)` pairs (the Rust counterpart of `ctx.ui.setStatus`).
    ///
    /// Deterministic order: plugin registration order, then key within each
    /// plugin. A key claimed by an earlier plugin wins over a later native
    /// plugin (first registration is authoritative; a duplicate is dropped so
    /// one key never renders twice).
    ///
    /// The bound [`PluginContext`]'s
    /// [`StatusService`](crate::plugins::plugin::services::StatusService)
    /// cells are merged in after the polled entries: a key set through the
    /// context replaces the polled text in place (the explicit live cell is
    /// the later write), and a new key is appended. The cell map itself
    /// implements the latest-set-wins rule for repeated writes of one key.
    ///
    /// This is the NATIVE half only. The JS runtime's statuses stay on their
    /// own notification channel (the TUI drains `Notification::Status` into
    /// its `extension_statuses` cell); merge the two with [`merge_statuses`]
    /// at the snapshot/footer assembly site. Hosts poll this when they build
    /// such a snapshot — not once per rendered frame.
    pub fn statuses(&self) -> Vec<(String, String)> {
        let mut statuses: Vec<(String, String)> = Vec::new();
        for plugin in &self.plugins {
            let mut entries = plugin.statuses();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for entry in entries {
                if statuses.iter().any(|(key, _)| *key == entry.0) {
                    continue;
                }
                statuses.push(entry);
            }
        }
        // Live status cells written through the bound context. Read once: the
        // context clone is an `Arc` bump and the service snapshot is one
        // allocation, same as the JS status drain.
        if let Some(context) = self.context() {
            if let Some(status) = context.status() {
                for (key, text) in status
                    .statuses()
                    .iter()
                    .map(|(key, text)| (key.to_string(), text.to_string()))
                {
                    match statuses.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(entry) => entry.1 = text,
                        None => statuses.push((key, text)),
                    }
                }
            }
        }
        statuses
    }

    /// The current shared registry snapshot.
    pub fn registry(&self) -> Arc<PluginRegistry> {
        self.registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The registry-definition generation: `0` after [`PluginHost::build`],
    /// incremented once for every successful rebuild
    /// ([`PluginHost::rebuild_registry`] / the change-gated
    /// [`PluginHost::refresh_registry_if_changed`]).
    ///
    /// A rebuild republishes definitions even when every tool keeps its name
    /// (a changed MCP description/schema or a JS re-registration), so
    /// consumers that cache a materialized tool registry must key on this
    /// counter rather than the name set. Reading it is one atomic load.
    pub fn registry_generation(&self) -> u64 {
        self.registry_generation.load(Ordering::Acquire)
    }

    /// Registry + load diagnostics of the last build.
    ///
    /// Consistent with [`Self::registration_failures`] by construction: a
    /// plugin that failed at build is off the dispatch list, so a later
    /// rebuild cannot re-observe it — the rebuild seeds these diagnostics
    /// with the standing failures ([`Self::standing_registration_failures`]),
    /// and a dispatch-list path that fails again is reported by its own pass.
    /// Each failure therefore appears exactly once, before and after a
    /// rebuild.
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        self.registry().diagnostics().to_vec()
    }

    /// Plugins whose most recent `register()` pass FAILED, as
    /// `(path, message)` in first-failure order.
    ///
    /// A failed registration is isolated (upstream per-extension factory
    /// error isolation): the plugin contributes nothing and never runs on the
    /// dispatch list. That isolation must not read as success — a host that
    /// specifically asked for the plugin (a `builtin:<name>` path, an explicit
    /// `-e` extension, an inline factory) reports this list instead of
    /// silently continuing without it.
    ///
    /// A build-time failure STANDS: `build` drops the plugin from the
    /// dispatch list, so a registry rebuild (and `/reload`) has no pass that
    /// could re-observe it, and the entry survives for as long as the plugin
    /// stays in the host's configuration. Only a pass that actually RE-RAN a
    /// path can clear its entry (a dispatch-list plugin whose later
    /// registration succeeds); a plugin that genuinely left the configuration
    /// starts clean in the new host [`Self::build`] returns.
    pub fn registration_failures(&self) -> Vec<RegistrationFailure> {
        self.registration_failures
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The ledger entries the next registration pass must carry over: the
    /// failures of plugins the pass does NOT re-run.
    ///
    /// A rebuild re-runs `register()` for the dispatch list only, and
    /// [`Self::build`] removed every plugin that failed from that list, so
    /// such a failure has no later pass that could re-observe it — dropping
    /// its entry would turn a real failure into a clean ledger. An entry for a
    /// path the pass DOES re-run is deliberately not carried: that pass's own
    /// result is authoritative (a transient failure may be gone, a new one is
    /// recorded).
    ///
    /// Configuration is immutable after build (the dispatch list is fixed),
    /// so a path that leaves the configuration is only ever observed as a new
    /// [`Self::build`]; there is no in-host removal for the ledger to follow.
    fn standing_registration_failures(&self) -> Vec<RegistrationFailure> {
        self.registration_failures()
            .into_iter()
            .filter(|failure| {
                !self
                    .plugins
                    .iter()
                    .any(|plugin| plugin.info().path == failure.path)
            })
            .collect()
    }

    /// Whether every plugin on the ordered list completed its registration
    /// pass. `false` means at least one requested plugin is absent from the
    /// registry; see [`Self::registration_failures`] for which and why.
    pub fn all_plugins_registered(&self) -> bool {
        self.registration_failures().is_empty()
    }

    /// Run every plugin's [`AgentPlugin::on_init`] hook exactly once, in the
    /// ordered plugin list, before the first dispatched event.
    ///
    /// This is the host call site the trait documents. Order matches every
    /// other lifecycle pass: the dispatch order [`Self::emit`] uses. Each
    /// plugin's context is already bound by the time a host calls this
    /// (`bind_context` runs at construction/rebind, and `on_context` fires
    /// before this hook), so an `on_init` implementation may use the typed
    /// services.
    ///
    /// Failure containment: a plugin that returns `Err` (or panics — the hook
    /// is awaited on this task, so a panic would otherwise unwind the host)
    /// is isolated into a load diagnostic and the remaining plugins still
    /// initialize. The returned vector is `(path, message)` in plugin order;
    /// empty means every plugin initialized.
    ///
    /// Idempotent: a second call (a rebind, a `/reload`, a host that calls it
    /// from more than one lifecycle seam) is a no-op. `/reload` re-runs
    /// [`AgentPlugin::on_reload`], not this.
    pub async fn run_init_hooks(&self) -> Vec<(String, String)> {
        if self.init_hooks_run.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }
        let mut failures: Vec<(String, String)> = Vec::new();
        for plugin in &self.plugins {
            match plugin.on_init().await {
                Ok(()) => {}
                Err(error) => {
                    let path = plugin.info().path.clone();
                    crate::ext_diag!("[plugins] plugin `{path}` init failed: {error}");
                    failures.push((path, error.to_string()));
                }
            }
        }
        failures
    }

    /// Whether [`Self::run_init_hooks`] has already run for this host.
    pub fn init_hooks_ran(&self) -> bool {
        self.init_hooks_run.load(Ordering::SeqCst)
    }

    /// The JS runtime handle, when extensions are configured.
    pub fn js_handle(&self) -> Option<&ExtensionHandle> {
        self.js.as_ref()
    }

    /// Mark this host as a borrower of another session's JS runtime: its
    /// [`Self::shutdown`] must not terminate the shared actor. Used by the
    /// subagent child when it reuses its parent's handle because a dedicated
    /// child runtime could not be started. Only the `subagent` builtin calls
    /// it; the flag itself stays compiled because [`Self::shutdown`] reads it
    /// in every build.
    #[cfg_attr(not(feature = "subagent"), allow(dead_code))]
    pub(crate) fn mark_js_shared(&self) {
        self.js_owned
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether any plugin (Rust or JS) registered a handler for `event`.
    ///
    /// True iff the JS runtime's registry snapshot holds at least one active
    /// handler for `event`, or some Rust plugin declares an interest that
    /// matches it. The QuickJS adapter is deliberately excluded from the
    /// Rust-interest scan: its [`EventInterest::All`] means "always consult
    /// the runtime for dispatch", so counting it as a wildcard would make
    /// this predicate vacuously true for every event as soon as any JS
    /// extension loads — turning every caller's "no consumer, skip the
    /// emission" fast path into a no-op. The runtime's own snapshot
    /// (`ExtensionHandle::has_handlers`) is the precise per-event answer and
    /// is checked first; the two checks mirror the dispatch's two consumers
    /// exactly.
    ///
    /// Invariant (out-of-band registrations): the JS snapshot is refreshed at
    /// each load and after every `emit`/`dispatch_command`/`invoke_shortcut`
    /// completion, and — counts-only — at the tail of the other JS entry
    /// points the actor runs (`call_tool`, `invoke_renderer`,
    /// `argument_completions`, `extension_suggestions`), so a handler
    /// registered from those contexts is visible before the next check. A
    /// `pi.on(...)` that runs OUTSIDE all of them (a `setTimeout`/promise
    /// continuation resumed with no command in flight, a JS tool `execute`
    /// body, a renderer/suggester callback) is covered by the generation pair
    /// instead: every handler-table mutation — a `pi.on` registration, its
    /// unsubscribe, the `refreshModels` row `pi.registerProvider` installs
    /// directly — bumps `handlers_gen` through the `handlersDirty` host fn, so
    /// a registration landing after a snapshot leaves
    /// `handlers_gen != handlers_snapshot_gen` and this predicate answers
    /// `true` until the next refresh re-publishes the counts. Inside that
    /// dirty window the gate is conservative: at most one emission whose
    /// event turns out to have no handler (harmless), never a skipped live
    /// handler.
    pub fn has_handlers(&self, event: &str) -> bool {
        if self.js.as_ref().is_some_and(|js| js.has_handlers(event)) {
            return true;
        }
        self.has_native_handlers(event)
    }

    /// Whether any NATIVE (non-JS) plugin declared an interest matching
    /// `event` — the Rust-only half of [`Self::has_handlers`].
    ///
    /// Emitters that dispatch the JS face through its own direct handle (the
    /// agent loop's `extensions` field) use this to decide whether the same
    /// event must ALSO be fanned out to the Rust plugins exactly once
    /// (see [`Self::emit_native`]). The QuickJS adapter is excluded for the
    /// same reason as in [`Self::has_handlers`]: its `EventInterest::All` is a
    /// "consult the runtime" marker, not a handler claim, and its events are
    /// already delivered through the direct handle.
    pub fn has_native_handlers(&self, event: &str) -> bool {
        // The interest table was snapshotted at build (see
        // [`NativeEventIndex`]); one map probe replaces the per-plugin
        // `events()` walk (which allocated a `Vec<String>` per `List`
        // plugin on every call).
        self.native_events.has_native_handlers(event)
    }

    /// Dispatch one event to the NATIVE (non-JS) plugin fan-out only: every
    /// matching Rust builtin/inline plugin in list order, payload mutations
    /// visible to later plugins, handler errors isolated into a diagnostic.
    ///
    /// This is [`Self::emit`] minus the QuickJS adapter. It exists for
    /// emitters that already send the event through the runtime's direct
    /// handle (`ExtensionHandle::emit` / `emit_stream_update`, the agent
    /// loop's `LoopDeps::extensions`) and would otherwise double-dispatch the
    /// JS face: the Rust half still has to observe every live event, including
    /// in a builtin-only plan where no JS handler exists at all.
    ///
    /// Interest is the declared [`AgentPlugin::events`] contract: a plugin is
    /// handed exactly the events it declares ([`EventInterest::List`] lists
    /// them, [`EventInterest::All`] admits every one), and the default
    /// [`EventInterest::None`] admits none — the same answer
    /// [`Self::has_native_handlers`] gives, which every caller pre-gates on.
    /// Callers normally gate the whole fan-out on
    /// [`Self::has_native_handlers`].
    ///
    /// No registry refresh runs here: only the JS definition snapshot can move
    /// between dispatch and the registry, and this entry never touches the JS
    /// runtime.
    pub async fn emit_native(&self, event: &str, payload: &mut JsonValue) {
        // A native-only fan-out may be the first thing a host dispatches
        // (`project_trust` runs during session assembly). Run the init hook
        // first, but only when the service context is already bound: `on_init`
        // is documented after `on_context`, and the SDK/CLI bind right after
        // construction. A pre-bind startup event leaves initialization to the
        // first `emit` (the first ordered dispatch).
        if !self.init_hooks_ran() && self.context().is_some() {
            let _ = self.run_init_hooks().await;
        }
        for index in self.native_events.dispatch_indices(event, false) {
            let plugin = &self.plugins[index];
            if let PluginEventOutcome::Error { error } =
                dispatch_native_event(plugin, event, payload).await
            {
                crate::ext_diag!("[ext] {event} native dispatch failed: {error}");
            }
        }
    }

    /// Dispatch one event through the ordered plugin list with the shared
    /// merge semantics. The payload may be mutated in place by handlers
    /// (`tool_call` input rewrites flow to later plugins and back to the
    /// host).
    pub async fn emit(&self, event: &str, payload: &mut JsonValue) -> anyhow::Result<EmitOutcome> {
        // Lifecycle: `on_init` runs exactly once, before the first ordered
        // dispatch (the trait contract). The atomic load keeps the per-token
        // hot path free of a compare-and-swap after the first run.
        if !self.init_hooks_ran() {
            let _ = self.run_init_hooks().await;
        }
        // `PI_TIMING=1`: one line per interesting lifecycle event, measured
        // once per process. The match runs before the env check so the hot
        // per-token events pay one string compare and nothing else.
        let event_started = matches!(
            event,
            "session_start" | "before_agent_start" | "input" | "resources_discover"
        )
        .then(crate::config::timings::start_phase)
        .flatten();
        let mut acc = MergedEmit::new();
        for index in self.native_events.dispatch_indices(event, true) {
            let plugin = &self.plugins[index];
            let outcome = dispatch_native_event(plugin, event, payload).await;
            acc.merge(event, &plugin.info().path, outcome);
            if acc.stop {
                break;
            }
        }
        self.refresh_registry_if_changed().await;
        // `input`: an in-place `payload` rewrite is part of the handler
        // contract (upstream handlers mutate the event they receive). The
        // typed `Input` outcome carries the fields the handler returned; fill
        // in any field it omitted from the final payload so an image list (or
        // text) mutated in place is not silently dropped by consumers that
        // read only the merged result.
        if event == "input"
            && acc.result.get("action").and_then(JsonValue::as_str) == Some("transform")
        {
            if let Some(result) = acc.result.as_object_mut() {
                if !result.contains_key("text") {
                    if let Some(text) = payload.get("text") {
                        result.insert("text".to_owned(), text.clone());
                    }
                }
                if !result.contains_key("images") {
                    if let Some(images) = payload.get("images") {
                        result.insert("images".to_owned(), images.clone());
                    }
                }
            }
        }
        if let Some(event_started) = event_started {
            let label = format!("ext.event.{event}");
            crate::config::timings::log_phase_once(&label, &label, event_started.elapsed());
        }
        Ok(EmitOutcome {
            result: acc.result,
            // The payload mirror rides `tool_call` only — the documented
            // contract of `EmitOutcome::payload` ("only populated for
            // `tool_call`", the event whose `input` rewrite the outcome
            // carries). Every other event's payload reaches its caller through
            // the `&mut` above, so cloning the (possibly large) event tree into
            // this field was pure per-event cost: the provider-stream hook
            // emits once per parsed provider event and discards the outcome.
            payload: if event == "tool_call" && payload.is_object() {
                payload.clone()
            } else {
                JsonValue::Null
            },
            errors: acc.errors,
        })
    }

    /// Whether the dispatch list contains any non-JS plugin.
    ///
    /// [`Self::emit`] dispatches the non-JS plugins in list order; a plugin
    /// observes exactly the events it declares ([`EventInterest::List`] lists
    /// them, [`EventInterest::All`] admits every one, the default
    /// [`EventInterest::None`] admits none). Emitters of informational
    /// events use this to decide whether the payload must be materialized as
    /// a `Value` for Rust consumers at all (see
    /// [`Self::emit_read_only_json`]).
    ///
    /// This is a COARSE check: it answers "does any native plugin exist", not
    /// "does any native plugin want THIS event". A host that attaches an
    /// always-present native plugin therefore answers `true` for every event.
    /// Event-specific emitters with an expensive payload (notably
    /// `session_tree`, the whole session forest) should gate on
    /// [`Self::has_native_handlers`] / [`Self::has_handlers`] for that event
    /// instead — a plugin that wants the event declares it in
    /// [`AgentPlugin::events`](crate::plugins::AgentPlugin::events).
    pub fn has_non_js_plugins(&self) -> bool {
        self.plugins.len() > self.native_events.js_adapters.len()
    }

    /// Dispatch an informational event whose handler return value no consumer
    /// reads, through the JS runtime's reply-free entry
    /// (`__pir_dispatchEventReadOnly`).
    ///
    /// `payload_json` is the payload as PRE-RENDERED JSON text. That is the
    /// point of this entry: [`Self::emit`]'s JS adapter receives a `Value`,
    /// and the runtime's reply carries the (possibly mutated) payload back, so
    /// a deep payload — `session_tree` carries the whole session forest —
    /// would be re-parsed, cloned and dropped one `serde_json` stack frame per
    /// nesting level. Here the text goes to JS as text and nothing comes back.
    ///
    /// `payload` is the same event payload for the non-JS plugins, which are
    /// dispatched exactly like [`Self::emit`] does (list order, an explicit
    /// [`EventInterest::List`] filters, in-place mutations visible to later
    /// plugins). It is `None` only when the caller knows the dispatch list is
    /// JS-only ([`Self::has_non_js_plugins`]); a missing payload for a non-JS
    /// plugin is reported and that plugin is skipped rather than handed a
    /// fabricated value.
    ///
    /// The JS runtime is asked for the precise per-event handler answer
    /// ([`ExtensionHandle::has_handlers`]) before paying a mailbox round trip:
    /// the adapter's [`EventInterest::All`] is a "consult the runtime" marker,
    /// not a wildcard handler claim. JS handler mutations are not written back
    /// to later Rust plugins — the read-only entry builds no reply by
    /// contract — which is exactly the semantics for the informational events
    /// this entry serves.
    pub async fn emit_read_only_json(
        &self,
        event: &str,
        mut payload_json: String,
        mut payload: Option<&mut JsonValue>,
    ) {
        // The JS adapter is handed the pre-rendered text by value. Count the
        // adapters that will actually receive it so the LAST one takes the
        // text by move: with the usual single adapter there is no clone at
        // all. `has_handlers` is consulted once here instead of once per JS
        // plugin in the loop.
        let js_dispatches = if self.js.as_ref().is_some_and(|js| js.has_handlers(event)) {
            self.native_events.js_adapters.len()
        } else {
            0
        };
        let mut js_seen = 0usize;
        let mut rust_visited = false;
        for index in self.native_events.dispatch_indices(event, true) {
            let plugin = &self.plugins[index];
            if self.native_events.js_adapters.binary_search(&index).is_ok() {
                if js_dispatches == 0 {
                    continue;
                }
                let Some(js) = &self.js else { continue };
                js_seen += 1;
                // A non-JS plugin that ran before the adapter may have
                // mutated the payload: later plugins in `emit` see that
                // view, so the JS side must too. The re-render is iterative.
                if rust_visited {
                    if let Some(value) = payload.as_deref() {
                        payload_json =
                            crate::session_manager::json_depth::write_value_iteratively(value);
                    }
                }
                let text = if js_seen == js_dispatches {
                    std::mem::take(&mut payload_json)
                } else {
                    payload_json.clone()
                };
                if let Err(error) = js.emit_read_only(event, text).await {
                    crate::ext_diag!("[ext] {event} dispatch failed: {error}");
                }
                continue;
            }
            let Some(value) = payload.as_deref_mut() else {
                crate::ext_diag!(
                    "[ext] {event} read-only dispatch has no payload for a non-JS plugin"
                );
                continue;
            };
            rust_visited = true;
            if let PluginEventOutcome::Error { error } =
                dispatch_native_event(plugin, event, value).await
            {
                crate::ext_diag!("[ext] {event} dispatch failed: {error}");
            }
        }
        self.refresh_registry_if_changed().await;
    }

    /// Re-run registration for every plugin on the dispatch list and rebuild
    /// the shared registry (after JS loads / runtime registrations). The
    /// dispatch list itself is fixed at build time (builtin omission is
    /// plan-driven and the JS adapter has a stable identity); rebuilds only
    /// refresh registrations. Plugins dropped from that list by a failed
    /// `register()` are not re-run — their [`Self::registration_failures`]
    /// entries stand, and the rebuild carries them into the new registry's
    /// diagnostics.
    pub async fn rebuild_registry(&self) {
        self.rebuild_registry_with(None).await;
    }

    /// [`Self::rebuild_registry`] with an already-read JS definition snapshot
    /// (the one [`Self::refresh_registry_if_changed`] compared); `None` reads
    /// a fresh one, which is the always-fresh path `/reload` takes.
    async fn rebuild_registry_with(&self, read: Option<JsDefSnapshot>) {
        // Seed the pass with the STANDING failures (see
        // [`Self::standing_registration_failures`]): a rebuild re-runs
        // `register()` for the DISPATCH list only, and `build` removed every
        // plugin that failed from that list, so those failures have no later
        // pass that could re-observe them. Deriving the ledger from this
        // vector alone (an empty seed) would write "clean" over a real
        // failure — the same class of defect as the earlier `mem::take`. The
        // seed also keeps [`Self::diagnostics`] consistent with the ledger:
        // ONE diagnostic per standing failure, exactly the text
        // [`registrations_failed`] reads back.
        let mut diagnostics: Vec<PluginDiagnostic> = self
            .standing_registration_failures()
            .into_iter()
            .map(|failure| PluginDiagnostic {
                kind: super::registry::DiagnosticKind::Error,
                path: failure.path,
                message: format!("{}{}", REGISTRATION_FAILURE_PREFIX, failure.message),
            })
            .collect();
        // Read the JS defs BEFORE the registration pass they feed: a
        // definition registered mid-pass then lands on the "changed" side of
        // the next check (an idempotent extra rebuild) instead of being baked
        // into the snapshot and silently skipped. The runtime reads also stay
        // outside the lock writes below.
        let snapshot = match (read, &self.js) {
            (Some(snapshot), _) => Some(snapshot),
            (None, Some(js)) => {
                // The generation is read BEFORE the defs (see
                // [`JsDefSnapshot::generation`]); a write landing between the
                // two leaves the next check on the "moved" side.
                let generation = self.js_def_generation();
                Some(JsDefSnapshot {
                    fingerprints: self.read_js_def_fingerprints(js),
                    generation,
                })
            }
            (None, None) => None,
        };
        let (registry, registration_failures) = build_registry(&self.plugins, &mut diagnostics);
        // The pass re-derived the ledger from the seeds above plus its own
        // results: a standing failure is still there, a dispatch-list path
        // that registers cleanly this time is gone, and a new error is
        // recorded. A rebuild can therefore never invent a clean ledger.
        *self
            .registration_failures
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = registration_failures;
        if let Some(snapshot) = snapshot {
            *self
                .js_defs
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        }
        *self
            .registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(registry);
        // Publish the snapshot before moving the generation (Release): a
        // reader that observes this generation with `Acquire` is guaranteed
        // to see the registry the rebuild just stored.
        self.registry_generation.fetch_add(1, Ordering::Release);
    }

    /// Consume the change signals and rebuild the registry when any moved,
    /// flushing the `mcp_servers_change` entries that rebuild queues.
    ///
    /// The signals that gate the rebuild:
    ///
    /// - the JS runtime's definition snapshot ([`JsDefFingerprints`], one
    ///   fingerprint per class [`JsExtensionsPlugin::register`] imports: JS
    ///   tool names, command name+description in export order, flag
    ///   name/type/description/default, shortcut key+description, provider
    ///   id+config, MCP-server name/config/extensionPath, full virtual-model
    ///   definitions) — so a handler that registers any of those without
    ///   changing its tool names is still published;
    /// - the `mcp` builtin manager's `tools_dirty` flag (MCP discovery
    ///   published a server's tool list).
    ///
    /// An unchanged snapshot is the idle case: no rebuild and no registration
    /// pass. The comparison is JS-only on both sides: the builtins re-register
    /// the same definitions every pass, so a full registry snapshot could
    /// never equal the JS defs and every check would rebuild (the
    /// always-rebuild this snapshot exists to skip). The rebuild re-commits
    /// the snapshot the pass consumed ([`Self::rebuild_registry_with`]), so
    /// the next check compares against what this pass actually consumed.
    ///
    /// # The gate and its O(1) fast path (invariant)
    ///
    /// The gate's guarantee is: **the registry is rebuilt exactly when a
    /// definition (or its order) changed, and a registration made during a
    /// dispatch is visible by the time `emit` returns.** Two halves keep it:
    ///
    /// - **Exact half** (always on): the defs are deep-read and compared
    ///   element-wise ([`JsDefFingerprints`]); only a real difference rebuilds.
    ///   A registration made during dispatch is already visible here: the
    ///   JS-registry classes were re-exported at the tail of the dispatch (the
    ///   runtime's own `registry_gen` early-out makes that export cheap and
    ///   skips it only when nothing was registered), and the classes written
    ///   straight through host fns (flags, providers, MCP servers, virtual
    ///   models) are read from runtime state directly.
    /// - **Cheap half** ([`DefGate`]): with a [`JsDefGenerationProbe`]
    ///   installed, an unchanged generation PROVES no definition write
    ///   happened since the committed snapshot was read, so the read is
    ///   skipped. A moved generation only re-enables the exact half — a
    ///   spurious bump (a renderer/widget registration the fingerprints do not
    ///   model) costs one read, never a rebuild. Without a probe the cheap
    ///   half cannot answer (`DefGate::Read(None)`) and the exact half runs on
    ///   every call, i.e. the pre-probe behavior.
    ///
    /// Soundness of the skip rests on the probe's contract
    /// ([`JsDefGenerationProbe`]): it must move on every write to a
    /// fingerprinted class. Only the runtime can know that (the writes happen
    /// behind its host fns), which is why the probe is installed by the
    /// runtime's owner and not derived here.
    ///
    /// Called at the end of every [`Self::emit`] and by the session's MCP
    /// wait hook ([`crate::agent_session::AgentSession::tool_call_wait_hook`])
    /// — the hook exists because the rebuild publishes pending MCP discovery
    /// (`tools_dirty`) and the narrowed `mcp` plugin interest leaves the
    /// emit-tail calls to events that some plugin actually handles.
    pub(crate) async fn refresh_registry_if_changed(&self) {
        let mut changed = false;
        // The snapshot this check read, re-committed when no rebuild consumes
        // it (`None` = the cheap half proved the committed one current).
        let mut read: Option<JsDefSnapshot> = None;
        if let Some(js) = &self.js {
            // The gate's cheap half first; every lock guard it takes ends
            // before the rebuild's await below.
            if let DefGate::Read(generation) = self.js_def_gate() {
                // Deep read outside the lock; compare under it.
                let fingerprints = self.read_js_def_fingerprints(js);
                let unchanged = {
                    let cached = self
                        .js_defs
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    cached.fingerprints == fingerprints
                };
                changed |= !unchanged;
                read = Some(JsDefSnapshot {
                    fingerprints,
                    generation,
                });
            }
        }
        #[cfg(feature = "mcp")]
        if let Some(manager) = &self.mcp_manager {
            // Discovered tools changed (session-start connect, or a
            // `tools/list_changed` refresh during a call): rebuild so the
            // freshly listed `mcp__*` tools join the registry.
            changed |= manager.take_tools_dirty();
        }
        if changed {
            self.rebuild_registry_with(read).await;
        } else if let Some(snapshot) = read {
            // No rebuild, but the defs were read: re-commit with the
            // generation observed before that read, so the cheap half latches
            // (a spurious probe bump must not cost a read per call) and the
            // snapshot stays the one a future comparison is against.
            *self
                .js_defs
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        }
        // The rebuild (or an earlier host-build registration pass) re-ran
        // `McpPlugin::register`, whose config sync and JS-def pass queue
        // `mcp_servers_change` entries. Flush at the enqueue point, ALWAYS:
        // the first `session_start` dispatch flushes the entries queued at
        // host build with no rebuild in between, and this call is where the
        // JS face (direct handle) and the native plugins (`emit_native`) get
        // their single copy each. An empty queue returns immediately, so the
        // no-change path only pays the follow-up/snapshot locks.
        #[cfg(feature = "mcp")]
        if let Some(mcp) = self.mcp_plugin() {
            mcp.flush_server_changes(self).await;
        }
    }

    /// The gate's O(1) half (see the invariant on
    /// [`Self::refresh_registry_if_changed`]): `Clean` only when an installed
    /// probe reports exactly the generation the committed snapshot was read at.
    fn js_def_gate(&self) -> DefGate {
        let Some(probe) = self.js_def_gen.as_ref() else {
            return DefGate::Read(None);
        };
        // Read the generation BEFORE the caller's defs read: a write landing
        // between the two keeps the next check on the "moved" side.
        let generation = probe();
        let cached = self
            .js_defs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.generation == Some(generation) {
            DefGate::Clean
        } else {
            DefGate::Read(Some(generation))
        }
    }

    /// The installed probe's current generation, when there is one.
    fn js_def_generation(&self) -> Option<u64> {
        self.js_def_gen.as_ref().map(|probe| probe())
    }

    /// Deep-read the JS definition snapshot — one clone of every definition
    /// class [`JsExtensionsPlugin::register`] imports. Counted by
    /// [`Self::js_def_fingerprint_reads`].
    fn read_js_def_fingerprints(&self, js: &ExtensionHandle) -> JsDefFingerprints {
        self.js_def_reads.fetch_add(1, Ordering::Relaxed);
        js_def_fingerprints(js)
    }

    /// How many deep JS-definition reads this host has performed (test probe
    /// for the gate: a clean emit with an installed [`JsDefGenerationProbe`]
    /// performs none).
    #[doc(hidden)]
    pub fn js_def_fingerprint_reads(&self) -> u64 {
        self.js_def_reads.load(Ordering::Relaxed)
    }

    /// Install (or clear) the O(1) definition-generation probe — the seam the
    /// runtime's owner (or a test) uses; see [`JsDefGenerationProbe`] for the
    /// contract the probe must satisfy.
    ///
    /// Installing one INVALIDATES the committed generation (never adopts the
    /// probe's current value): the snapshot was read before the install, so a
    /// write landing in between would otherwise be skipped. The next check
    /// therefore deep-reads once and latches from there.
    #[doc(hidden)]
    pub fn set_js_def_generation_probe(&mut self, probe: Option<JsDefGenerationProbe>) {
        self.js_def_gen = probe;
        self.js_defs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation = None;
    }

    /// The `mcp` builtin's shared manager, when the plugin is loaded (host
    /// seam for status surfaces and tests).
    #[cfg(feature = "mcp")]
    pub fn mcp_manager(&self) -> Option<&McpManagerHandle> {
        self.mcp_manager.as_ref()
    }

    /// The `mcp` builtin in the dispatch list, when loaded — the downcast
    /// used by the rebuild-time `mcp_servers_change` flush ([`Self::reload`]
    /// and [`Self::refresh_registry_if_changed`]).
    #[cfg(feature = "mcp")]
    fn mcp_plugin(&self) -> Option<&McpPlugin> {
        let index = self.mcp_plugin_index?;
        self.plugins
            .get(index)?
            .as_any()?
            .downcast_ref::<McpPlugin>()
    }

    /// First-wins tool definitions across the unified registry.
    pub fn tool_defs(&self) -> Vec<crate::extensions::ToolDef> {
        self.registry()
            .resolved_tools()
            .into_iter()
            .map(|(def, _)| def.clone())
            .collect()
    }

    /// Execute a registered tool by name through its backend (Rust tool
    /// object or QuickJS) and return only the JS-wire result.
    ///
    /// A native Rust tool runs under a minimal [`ToolContext`]
    /// ([`ToolContext::new`] at the host cwd): callers that already hold the
    /// executing call's context must use [`Self::call_tool_with_context`] so
    /// the tool sees the real abort signal, session environment, nested-call
    /// capability, update channel and background flag.
    ///
    /// [`ToolContext`]: crate::agent::tool_execution::ToolContext
    /// [`ToolContext::new`]: crate::agent::tool_execution::ToolContext::new
    pub async fn call_tool(
        &self,
        name: &str,
        tool_call_id: &str,
        params: &JsonValue,
    ) -> anyhow::Result<ToolResult> {
        let ctx = crate::agent::tool_execution::ToolContext::new(self.cwd.clone());
        Ok(self
            .call_tool_with_context(name, tool_call_id, params, &ctx)
            .await?
            .result)
    }

    /// Execute a registered tool by name with the CALLER'S full
    /// [`ToolContext`](crate::agent::tool_execution::ToolContext).
    ///
    /// This is the native-call seam: a Rust plugin tool runs exactly like a
    /// core tool (same abort signal, session environment, nested executor,
    /// live update channel and background flag) instead of through a
    /// synthetic context carrying only the cwd. The resolution order is the
    /// unified registry's (ordered plugin list, first registration per name
    /// wins), so a native call is dispatched by the same order the model's
    /// calls use.
    ///
    /// Image fidelity: a native tool's result images are returned both in
    /// [`ToolCallOutcome::images`] and as `{type: "image"}` content items, so
    /// neither the structured return nor the JS-wire result silently drops
    /// them.
    pub async fn call_tool_with_context(
        &self,
        name: &str,
        tool_call_id: &str,
        params: &JsonValue,
        ctx: &crate::agent::tool_execution::ToolContext,
    ) -> anyhow::Result<ToolCallOutcome> {
        // Only the backend is needed here. The old path materialized the
        // whole resolved tool list (a `Vec` + a name set per call) and cloned
        // the matching `ToolDef` (schema JSON included) on every plugin tool
        // call; `tool_backend` answers the same first-wins question with no
        // allocation, and the backend clone is the cheap `Arc`/enum bump
        // `ToolBackend` already derives.
        let registry = self.registry();
        let Some(backend) = registry.tool_backend(name).cloned() else {
            anyhow::bail!("tool `{name}` is not registered by any plugin");
        };
        match backend {
            ToolBackend::Rust(tool) => {
                let result = tool.run(params, ctx).await;
                if result.is_error {
                    Err(anyhow::anyhow!("tool `{name}` failed: {}", result.content))
                } else {
                    // `tool.run` hands the result to this function with sole
                    // ownership, so move the payload fields into the wire
                    // result: the text, details JSON and structured JSON are
                    // the large objects on this path and were deep-cloned for
                    // no reason (the caller drops the native result here).
                    let crate::agent::tool_batch::ToolResult {
                        content: text,
                        images,
                        details,
                        structured_content,
                        terminate,
                        ..
                    } = result;
                    let mut content = Vec::with_capacity(1 + images.len());
                    content.push(crate::extensions::ContentItem::text(text));
                    // Images are the one dual-representation field: the same
                    // block must appear both in `images` and as a wire
                    // `{type:"image"}` item, and `ImageContent` owns its
                    // base64 `String`, so this copy is required (not a
                    // redundant clone).
                    for image in &images {
                        content.push(crate::extensions::ContentItem::image(
                            image.data.clone(),
                            image.mime_type.clone(),
                        ));
                    }
                    Ok(ToolCallOutcome {
                        result: ToolResult {
                            content,
                            details: details.unwrap_or(JsonValue::Null),
                            structured_content: structured_content.unwrap_or(JsonValue::Null),
                            terminate,
                        },
                        images,
                    })
                }
            }
            ToolBackend::Js { .. } => {
                let Some(js) = &self.js else {
                    anyhow::bail!("tool `{name}` has no JS runtime");
                };
                let result = js.call_tool(name, tool_call_id, params).await?;
                // A JS tool's image blocks already travel in `content`
                // (`{type:"image", data, mimeType}`); surface them in the
                // structured return as well.
                let images = result
                    .content
                    .iter()
                    .filter(|item| item.kind == "image")
                    .filter_map(|item| {
                        Some(crate::ai::types::ImageContent {
                            data: item.data.clone()?,
                            mime_type: item
                                .mime_type
                                .clone()
                                .unwrap_or_else(|| "image/png".to_owned()),
                        })
                    })
                    .collect();
                Ok(ToolCallOutcome { result, images })
            }
        }
    }

    /// Execute a slash command by resolved invocation name. Rust command
    /// handlers run natively; JS commands dispatch through the runtime.
    pub async fn dispatch_command(
        &self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<crate::extensions::CommandOutcome> {
        // Resolve the one matching command from the build-time table; the old
        // path materialized and cloned EVERY command just to find it.
        let resolved = self.registry().find_command(name);
        let Some(command) = resolved else {
            let Some(js) = &self.js else {
                anyhow::bail!("command `{name}` is not registered by any plugin");
            };
            return js.dispatch_command(name, args).await;
        };
        match command.backend {
            Some(CommandBackend::Rust(handler)) => {
                let outcome = handler(args).await;
                // A native command may change plugin state that only a
                // rebuild publishes (the `mcp` builtin's enable/disable/
                // exposure/remove mark the manager dirty). JS commands keep
                // their established dispatch path: the runtime's own emit
                // tail refreshes the registry.
                self.refresh_registry_if_changed().await;
                Ok(outcome)
            }
            Some(CommandBackend::Js) | None => {
                let Some(js) = &self.js else {
                    anyhow::bail!("command `{name}` has no JS runtime");
                };
                js.dispatch_command(&command.def.name, args).await
            }
        }
    }

    /// Invoke a keyboard shortcut (unified resolution happens in the
    /// registry; the TUI calls this with a resolved binding's key).
    pub async fn invoke_shortcut(
        &self,
        key: &str,
    ) -> anyhow::Result<crate::extensions::CommandOutcome> {
        // Borrow the matching registration (last wins, the same answer
        // `resolve_shortcuts` gives with no reserved-key overrides) instead of
        // cloning and sorting the whole shortcut set per keypress.
        let registry = self.registry();
        let resolved = registry
            .shortcut(key)
            .and_then(|(_, backend)| backend.cloned());
        match resolved {
            Some(ShortcutBackend::Rust(handler)) => Ok(handler().await),
            Some(ShortcutBackend::Js) | None => {
                let Some(js) = &self.js else {
                    anyhow::bail!("shortcut `{key}` has no JS runtime");
                };
                js.invoke_shortcut(key).await
            }
        }
    }

    /// Emit `session_shutdown {reason}` to the NATIVE plugins only.
    ///
    /// Upstream tears the extension runtime down with `session_shutdown` on
    /// quit and reload; the JS face receives its own event from the actor
    /// lifecycle ([`ExtensionHandle::shutdown`] for quit,
    /// [`ExtensionHandle::reload`] for reload), so this helper must never
    /// reach the JS adapter — [`Self::emit_native`] skips it, keeping the
    /// event exactly-once on both faces.
    async fn emit_native_session_shutdown(&self, reason: &str, target_session_file: Option<&str>) {
        if !self.has_native_handlers("session_shutdown") {
            return;
        }
        let mut payload = session_shutdown_payload(reason, target_session_file);
        self.emit_native("session_shutdown", &mut payload).await;
    }

    /// Enter one native UI prompt, returning `true` when it is the OUTERMOST
    /// active prompt. Only then does the caller emit `ui_prompt_start`
    /// (upstream `withUIPrompt`: `const outerPrompt = this.uiPromptDepth++
    /// === 0`, runner.ts:589-593).
    pub fn begin_ui_prompt(&self, kind: &str, title: &str) -> bool {
        let mut state = self
            .ui_prompt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_add(1);
        if state.0 == 1 {
            state.1 = Some((kind.to_owned(), title.to_owned()));
            true
        } else {
            false
        }
    }

    /// Leave one native UI prompt; `Some((kind, title))` when this was the
    /// outermost prompt, whose identity the single `ui_prompt_end` carries
    /// (upstream's `finish()` reads `activeUIPrompt`, not the closing inner
    /// dialog's fields). An unbalanced call is a no-op (`None`).
    pub fn end_ui_prompt(&self) -> Option<(String, String)> {
        let mut state = self
            .ui_prompt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.0 == 0 {
            return None;
        }
        state.0 -= 1;
        if state.0 == 0 {
            state.1.take()
        } else {
            None
        }
    }

    /// Emit `session_shutdown {reason, targetSessionFile?}` to BOTH faces for
    /// a session replacement (upstream `teardownCurrent`, agent-session-
    /// runtime.ts:167-177): the outgoing runtime is torn down while the
    /// destination session file is already known.
    ///
    /// Unlike the quit/reload teardown ([`Self::shutdown`] /
    /// [`Self::reload`]), a session switch has no JS actor lifecycle of its
    /// own, so this goes through the unified [`Self::emit`] — each face
    /// observes the event exactly once.
    pub async fn emit_session_shutdown(&self, reason: &str, target_session_file: Option<&str>) {
        if !self.has_handlers("session_shutdown") {
            return;
        }
        let mut payload = session_shutdown_payload(reason, target_session_file);
        if let Err(error) = self.emit("session_shutdown", &mut payload).await {
            crate::ext_diag!("[ext] session_shutdown dispatch failed: {error}");
        }
    }

    /// Re-run every plugin's `on_reload` hook plus the JS `/reload`
    /// (lifecycle parity with the JS runtime).
    ///
    /// Native plugins observe the upstream teardown event first
    /// (`session_shutdown {reason: "reload"}`), then their `on_reload` hook —
    /// the same order `shutdown` applies for `quit`. The JS face gets its own
    /// reload lifecycle from the actor (`ExtensionHandle::reload`).
    ///
    /// A plugin whose `on_reload` fails is logged and does NOT abort the pass:
    /// every plugin still gets its hook, the JS reload still runs and the
    /// registry is still rebuilt — the same policy [`Self::shutdown`] applies
    /// to a failing `on_shutdown`. A failed JS reload still returns its error
    /// (the actor kept the previous runtime usable), before the rebuild: the
    /// rebuild would re-publish a registry built from a runtime that did not
    /// move.
    pub async fn reload(&self) -> anyhow::Result<()> {
        self.run_native_reload_hooks().await;
        if let Some(js) = &self.js {
            js.reload().await?;
        }
        self.publish_reload().await;
        Ok(())
    }

    /// The NATIVE half of [`Self::reload`], for hosts that drive the JS half
    /// themselves.
    ///
    /// The interactive host's extension dispatcher owns
    /// [`ExtensionHandle::reload`] (its status/error reporting and the UI's
    /// stale-status clearing are UI concerns), so its `/reload` reaches the
    /// plugin state through here — one hook pass, one rebuild, and the JS
    /// runtime is left to the dispatcher instead of being reloaded twice per
    /// command. Product caller:
    /// [`AgentSession::reload_plugins`](crate::agent_session::AgentSession::reload_plugins).
    /// [`Self::reload`] remains the composite entry point for hosts that have
    /// no dispatcher (the SDK, RPC, tests).
    pub async fn reload_native_plugins(&self) {
        self.run_native_reload_hooks().await;
        self.publish_reload().await;
    }

    /// `session_shutdown {reason: "reload"}` to the native plugins, then every
    /// plugin's `on_reload`. Failures are diagnostics, never an early return:
    /// one broken plugin must not keep the others (or the registry rebuild)
    /// from reloading.
    async fn run_native_reload_hooks(&self) {
        self.emit_native_session_shutdown("reload", None).await;
        for plugin in &self.plugins {
            if let Err(error) = plugin.on_reload().await {
                crate::ext_diag!(
                    "[plugins] plugin `{}` reload failed: {error}",
                    plugin.info().path
                );
            }
        }
    }

    /// Publish a completed reload: rebuild the registry (re-running every
    /// `register()`, so a reloaded config — codemode's settings, the `mcp`
    /// manager's torn-down connections, `todo`'s cleared replay cache — takes
    /// effect) and flush the `mcp_servers_change` entries that rebuild queued.
    async fn publish_reload(&self) {
        self.rebuild_registry().await;
        // The rebuild re-ran `McpPlugin::register`; flush the
        // `mcp_servers_change` entries it queued at the enqueue point (the
        // same JS + native fan-out the refresh tail uses).
        #[cfg(feature = "mcp")]
        if let Some(mcp) = self.mcp_plugin() {
            mcp.flush_server_changes(self).await;
        }
    }

    /// Run every plugin's `on_shutdown` hook exactly once (idempotent — the
    /// one-shot guard makes teardown racing double-fire a no-op). This is the
    /// host teardown path that makes plugin resource release reachable
    /// outside the SDK: the `mcp` builtin's graceful stdio close and process
    /// tree sweep run from here at session teardown.
    ///
    /// `session_shutdown {reason: "quit"}` reaches both faces before any
    /// `on_shutdown` hook: native plugins through the direct emit, the JS
    /// runtime through [`ExtensionHandle::shutdown`], which also stops the
    /// extension actor thread. A host borrowing another session's runtime
    /// ([`Self::mark_js_shared`]) skips the JS half.
    pub async fn shutdown(&self) {
        if self
            .shut_down
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.emit_native_session_shutdown("quit", None).await;
        if self.js_owned.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(js) = &self.js {
                if let Err(error) = js.shutdown().await {
                    crate::ext_diag!("[plugins] JS runtime shutdown failed: {error}");
                }
            }
        }
        for plugin in &self.plugins {
            if let Err(error) = plugin.on_shutdown().await {
                crate::ext_diag!(
                    "[plugins] plugin `{}` shutdown failed: {error}",
                    plugin.info().path
                );
            }
        }
    }

    // -- plumbing pass-throughs (session/TUI hosts) --------------------------

    /// The persisted `setActiveTools` override, if any.
    pub fn active_tools_override(&self) -> Option<Vec<String>> {
        self.js.as_ref().and_then(|js| js.active_tools_override())
    }

    /// Clear the extension abort state at the start of a fresh run.
    pub async fn clear_abort(&self) {
        if let Some(js) = &self.js {
            js.clear_abort().await;
        }
    }

    /// Abort the in-flight extension tool call.
    pub fn abort_current_call(&self) {
        if let Some(js) = &self.js {
            js.abort_current_call();
        }
    }

    /// Drain notifications matching `predicate`, keeping the rest queued.
    pub fn take_notifications(
        &self,
        predicate: impl Fn(&crate::extensions::Notification) -> bool,
    ) -> Vec<crate::extensions::Notification> {
        match &self.js {
            Some(js) => js.take_notifications(predicate),
            None => Vec::new(),
        }
    }

    /// How many notifications are currently queued in the JS runtime.
    pub fn pending_notification_count(&self) -> usize {
        self.js
            .as_ref()
            .map(|js| js.pending_notification_count())
            .unwrap_or(0)
    }

    /// Push the live session/model snapshot backing the `ctx` facade.
    ///
    /// Two consumers share one push: the ordered Rust plugin list
    /// ([`AgentPlugin::on_session_context`] — the `tool-repair` builtin's
    /// model-id reach for its `before_provider_request` gate) and the JS
    /// runtime handle the host owns. The Rust fan-out runs FIRST and
    /// independently of the JS handle, so a builtin/inline-only session (no
    /// extension runtime) still reaches its plugins.
    ///
    /// The snapshot is wrapped in one `Arc` per push and retained for
    /// [`Self::session_context_snapshot`] /
    /// [`Self::session_entries_snapshot`]; the native fan-out borrows it, and
    /// the JS runtime receives one clone only when a runtime exists (a
    /// Rust-only host never clones the payload).
    pub fn update_session_context(&self, snapshot: crate::extensions::SessionContextSnapshot) {
        let snapshot = Arc::new(snapshot);
        // Store before notifying: an `on_session_context` implementation may
        // read the service's snapshot and must observe this push.
        *self
            .session_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&snapshot));
        *self
            .session_entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::new(snapshot.entries.clone()));
        for plugin in &self.plugins {
            plugin.on_session_context(&snapshot);
        }
        if let Some(js) = &self.js {
            // Share the SAME snapshot `Arc` with the JS runtime: it keeps the
            // value for the next prompt, and the native readers above already
            // hold a reference, so there is nothing to deep-copy (the old
            // `snapshot.as_ref().clone()` duplicated the whole session-entry
            // JSON on every push).
            js.update_session_context_shared(Arc::clone(&snapshot));
        }
    }

    /// The last session-context snapshot pushed through
    /// [`Self::update_session_context`], when any.
    ///
    /// Repeated reads return the same `Arc` — one allocation per push — so a
    /// reader (the live [`SessionService`](crate::plugins::plugin::services::SessionService)
    /// binding) can hand the typed snapshot out without re-serializing.
    pub fn session_context_snapshot(&self) -> Option<Arc<SessionContextSnapshot>> {
        self.session_context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The `entries` payload of the last pushed session-context snapshot, as
    /// its own `Arc`.
    ///
    /// This is the value `ctx.sessionManager.getEntries()` reflects; retaining
    /// it separately means `SessionService::entries()` returns the pushed
    /// `Arc` instead of deep-cloning it out of the snapshot.
    pub fn session_entries_snapshot(&self) -> Option<Arc<JsonValue>> {
        self.session_entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn update_model_registry(&self, models: JsonValue, providers: JsonValue) {
        if let Some(js) = &self.js {
            js.update_model_registry(models, providers);
        }
    }

    pub fn update_scoped_models(&self, scoped: JsonValue) {
        if let Some(js) = &self.js {
            js.update_scoped_models(scoped);
        }
    }

    pub fn set_thinking_level_value(&self, level: &str) {
        if let Some(js) = &self.js {
            js.set_thinking_level_value(level);
        }
    }

    pub fn set_system_prompt_value(&self, prompt: &str) {
        if let Some(js) = &self.js {
            js.set_system_prompt_value(prompt);
        }
    }

    pub fn set_call_timeout(&self, timeout: std::time::Duration) {
        if let Some(js) = &self.js {
            js.set_call_timeout(timeout);
        }
    }
}

/// Merge native plugin statuses into the JS extension status snapshot.
///
/// `js` is the runtime's ordered status list (the `ctx.ui.setStatus`
/// notifications the TUI upserts by key); `native` is
/// [`PluginHost::statuses`] (plugin registration order, then key). Entries
/// with DISTINCT keys from both sources are kept.
///
/// Same-key precedence: the NATIVE entry wins. A builtin plugin is the
/// authoritative owner of the key it publishes, and the phone builtin is
/// mutually exclusive with its JS face at the loader (a `builtin:phone` plan
/// drops the on-disk JS copy), so a real collision cannot occur in practice;
/// the rule exists so a stale JS text can never shadow the live native state
/// if one ever does. A native entry with a key already claimed by an earlier
/// native plugin keeps the earlier one (see [`PluginHost::statuses`]).
pub fn merge_statuses(
    js: &[(String, String)],
    native: &[(String, String)],
) -> Vec<(String, String)> {
    let mut merged: Vec<(String, String)> = js.to_vec();
    for (key, text) in native {
        match merged.iter_mut().find(|(existing, _)| existing == key) {
            Some(slot) => slot.1 = text.clone(),
            None => merged.push((key.clone(), text.clone())),
        }
    }
    merged
}

/// Whether `plugin` is the QuickJS extensions adapter — the one plugin whose
/// [`EventInterest::All`] is a "consult the runtime" marker, not a wildcard
/// handler claim (see [`PluginHost::has_handlers`]).
fn is_js_adapter(plugin: &Arc<dyn AgentPlugin>) -> bool {
    plugin
        .as_any()
        .is_some_and(|any| any.is::<JsExtensionsPlugin>())
}

/// Every native fan-out entry (`[`PluginHost::emit`]`, `emit_native`,
/// `emit_read_only_json`, terminal input) uses this table, so they all agree
/// with [`PluginHost::has_native_handlers`]: [`EventInterest::List`] admits
/// exactly the listed names, [`EventInterest::All`] admits every name, and the
/// default [`EventInterest::None`] admits none.
///
/// The one deliberate exception to the declaration fence is
/// [`crate::agent::tool_execution::dispatch_hosted_event`] (`tool_call`,
/// `tool_result`, `user_bash`), which walks every non-JS plugin directly and
/// must keep doing so: `todo`'s idempotent `tool_result` REPLACE arm is
/// reachable only through it.
fn js_tool_names(js: &ExtensionHandle) -> Vec<String> {
    let mut names: Vec<String> = js.tool_defs().into_iter().map(|def| def.name).collect();
    names.sort();
    names
}

/// One `{name, description}` fingerprint per exported JS command, in export
/// order (see [`JsDefFingerprints::commands`]).
fn js_command_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    js.command_defs()
        .into_iter()
        .map(|def| {
            serde_json::json!({
                "name": def.name,
                "description": def.description,
            })
            .to_string()
        })
        .collect()
}

/// Test seam: the command-definition fingerprint vector the gate compares
/// ([`js_command_fingerprints`]), exposed so a test can pin that it is
/// order-SENSITIVE. Duplicate command names resolve to `name:2`, `name:3`, ...
/// by occurrence, so the same definition set exported in a different order is
/// a real registry change — a set-based (sorted) comparison would miss it.
#[doc(hidden)]
pub fn command_def_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    js_command_fingerprints(js)
}

/// One `{name, type, description, default}` fingerprint per registered JS
/// flag, sorted by name (see [`JsDefFingerprints::flags`]).
fn js_flag_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    let mut fingerprints: Vec<String> = js
        .flags()
        .into_iter()
        .map(|flag| {
            serde_json::json!({
                "name": flag.name,
                "type": flag.flag_type,
                "description": flag.description,
                "default": flag.default,
            })
            .to_string()
        })
        .collect();
    fingerprints.sort();
    fingerprints
}

/// One `{key, description}` fingerprint per registered JS shortcut, sorted by
/// key (see [`JsDefFingerprints::shortcuts`]).
fn js_shortcut_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    let mut fingerprints: Vec<String> = js
        .shortcuts()
        .into_iter()
        .map(|shortcut| {
            serde_json::json!({
                "key": shortcut.key,
                "description": shortcut.description,
            })
            .to_string()
        })
        .collect();
    fingerprints.sort();
    fingerprints
}

/// One `{id, config}` fingerprint per `pi.registerProvider` definition,
/// sorted (see [`JsDefFingerprints::providers`]). Mirrors the registration
/// pass: a def without a string `id` registers nothing and is skipped here
/// too.
fn js_provider_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    let mut fingerprints: Vec<String> = js
        .provider_defs()
        .iter()
        .filter_map(|def| {
            let id = def.get("id").and_then(JsonValue::as_str)?;
            Some(
                serde_json::json!({
                    "id": id,
                    "config": def.get("config").cloned().unwrap_or(JsonValue::Null),
                })
                .to_string(),
            )
        })
        .collect();
    fingerprints.sort();
    fingerprints
}

/// One `{name, config, extensionPath}` fingerprint per `pi.registerMcpServer`
/// definition, sorted (see [`JsDefFingerprints::mcp_servers`]). Mirrors the
/// `mcp` builtin's pass: a def without a string `name` registers nothing and
/// is skipped here too; the config is part of the fingerprint so a same-name
/// config change still rebuilds.
fn js_mcp_server_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    let mut fingerprints: Vec<String> = js
        .mcp_server_defs()
        .iter()
        .filter_map(|def| {
            let name = def.get("name").and_then(JsonValue::as_str)?;
            Some(
                serde_json::json!({
                    "name": name,
                    "config": def.get("config").cloned().unwrap_or(JsonValue::Null),
                    "extensionPath": def
                        .get("extensionPath")
                        .cloned()
                        .unwrap_or(JsonValue::Null),
                })
                .to_string(),
            )
        })
        .collect();
    fingerprints.sort();
    fingerprints
}

/// One full-definition fingerprint per `pi.registerVirtualModel` definition,
/// sorted (see [`JsDefFingerprints::virtual_models`]).
fn js_virtual_model_fingerprints(js: &ExtensionHandle) -> Vec<String> {
    let mut fingerprints: Vec<String> = js
        .virtual_model_defs()
        .iter()
        .map(|definition| definition.to_string())
        .collect();
    fingerprints.sort();
    fingerprints
}

/// The JS runtime's current definition snapshot — every class
/// [`JsExtensionsPlugin::register`] imports, in the shape
/// [`PluginHost::refresh_registry_if_changed`] compares and
/// [`PluginHost::rebuild_registry`] commits.
fn js_def_fingerprints(js: &ExtensionHandle) -> JsDefFingerprints {
    JsDefFingerprints {
        tools: js_tool_names(js),
        commands: js_command_fingerprints(js),
        flags: js_flag_fingerprints(js),
        shortcuts: js_shortcut_fingerprints(js),
        providers: js_provider_fingerprints(js),
        mcp_servers: js_mcp_server_fingerprints(js),
        virtual_models: js_virtual_model_fingerprints(js),
    }
}

/// Register every plugin and build the registry.
///
/// Returns the registry plus the registration-failure ledger for THIS pass.
/// The ledger must be computed here, before
/// [`PluginRegistry::build`] takes ownership of `diagnostics`: that call
/// drains the vector into the registry, so a caller reading the shared
/// `diagnostics` afterwards would always see an empty list (the failure this
/// function's return value fixes).
///
/// `diagnostics` may arrive non-empty: a rebuild seeds it with the standing
/// failures of plugins it does not re-run (see
/// [`PluginHost::standing_registration_failures`]). Those seeds are derived
/// back into the ledger by [`registrations_failed`], so seeding is how the
/// ledger and the registry diagnostics stay one set instead of two.
fn build_registry(
    plugins: &[Arc<dyn AgentPlugin>],
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> (PluginRegistry, Vec<RegistrationFailure>) {
    let mut candidates: Vec<RegisteredPlugin> = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        let mut registrations = super::plugin::PluginRegistrations::default();
        let result = {
            let mut ctx = RegistrationContext::new(&mut registrations);
            plugin.register(&mut ctx)
        };
        match result {
            Ok(()) => candidates.push(RegisteredPlugin {
                info: plugin.info().clone(),
                registrations,
            }),
            Err(error) => diagnostics.push(PluginDiagnostic {
                kind: super::registry::DiagnosticKind::Error,
                path: plugin.info().path.clone(),
                message: format!("{}{}", REGISTRATION_FAILURE_PREFIX, error),
            }),
        }
    }
    let registration_failures = registrations_failed(diagnostics);
    let registry = PluginRegistry::build(candidates, std::mem::take(diagnostics));
    (registry, registration_failures)
}

/// The diagnostic text [`build_registry`] writes for a failed registration,
/// and the prefix [`registrations_failed`] reads back. One constant so the
/// encode and the decode cannot drift (a rebuild re-encodes a standing
/// [`RegistrationFailure`] with it).
const REGISTRATION_FAILURE_PREFIX: &str = "plugin failed to load: ";

/// The load diagnostics that describe a FAILED registration (the same
/// `plugin failed to load:` text [`build_registry`] records), deduped per path
/// in first-failure order. Conflict/replacement diagnostics and unknown
/// `builtin:` names are not registration failures and stay out.
fn registrations_failed(diagnostics: &[PluginDiagnostic]) -> Vec<RegistrationFailure> {
    let mut failures: Vec<RegistrationFailure> = Vec::new();
    for diagnostic in diagnostics {
        if diagnostic.kind != super::registry::DiagnosticKind::Error {
            continue;
        }
        let Some(message) = diagnostic.message.strip_prefix(REGISTRATION_FAILURE_PREFIX) else {
            continue;
        };
        if failures
            .iter()
            .any(|failure| failure.path == diagnostic.path)
        {
            continue;
        }
        failures.push(RegistrationFailure {
            path: diagnostic.path.clone(),
            message: message.to_owned(),
        });
    }
    failures
}

/// Credential-environment disk loads performed by the live plugin services'
/// `has_auth` oracle since the last reset.
///
/// `#[doc(hidden)]` test/profiling probe: the live adapter (in
/// `agent_session::build::plugin_services`) increments this once per cold read
/// or background refresh, and a test pins "one read for N hot-path calls".
static LIVE_AUTH_DISK_READS: AtomicU64 = AtomicU64::new(0);

/// Record one credential-environment disk load (live services only).
pub(crate) fn record_live_auth_disk_read() {
    LIVE_AUTH_DISK_READS.fetch_add(1, Ordering::Relaxed);
}

/// Credential-environment disk loads performed by the live
/// [`ProviderService`](crate::plugins::services::ProviderService) binding
/// (`#[doc(hidden)]` test/profiling probe).
#[doc(hidden)]
pub fn live_auth_disk_reads() -> u64 {
    LIVE_AUTH_DISK_READS.load(Ordering::Relaxed)
}

/// Reset [`live_auth_disk_reads`] (`#[doc(hidden)]` test probe).
#[doc(hidden)]
pub fn reset_live_auth_disk_reads() {
    LIVE_AUTH_DISK_READS.store(0, Ordering::Relaxed);
}
