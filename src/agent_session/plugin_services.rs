//! Live, Rust-native [`PluginContext`] binding for an [`AgentSession`].
//!
//! This module is the production implementation of the service spine declared
//! in [`crate::plugins::plugin::services`]: it builds one [`PluginServices`]
//! set from the live session and
//! [`PluginHost::bind_context`](crate::plugins::PluginHost::bind_context) hands
//! it to every native plugin once per construction. The JS face keeps its
//! existing host functions; nothing here re-serializes typed values.
//!
//! # What is live
//!
//! | Service | Backing |
//! |---|---|
//! | [`SessionService`] | session `Arc`s (cwd, model/thinking cells, idle/pending cells, scoped models, tool registry) plus the plugin host's live registry |
//! | [`MessageService`] | a bounded native-action queue on [`AgentSession`] drained by `apply_extension_notifications` through the SAME `consume_send_message` / `consume_send_user_message` / `consume_append_entry` handlers as the JS host functions; the queued payloads are byte-identical to the JS wire shapes |
//! | [`ToolService`] | `callable_tools` from the session's merged-registry memo; `execute_tool` through `PluginHost::call_tool_with_context` with the session's live [`ToolContext`] (abort signal, session environment, nested-call executor, update channel, background flag, cwd) for plugin tools and the merged registry with the same context for core tools; image blocks survive into the returned result; `exec` as a direct subprocess with cwd/timeout/abort; `update` through the one `tool_execution_update` dispatcher |
//! | [`UiService`] | the JS runtime's bounded notification queue (shared with `ctx.ui.*`), bound only when that presentation sink exists; `select`/`confirm`/`input`/`editor` enqueue the SAME `DialogRequest` notification the JS host fn produces and await a one-shot reply registered on the host's native dialog cell, resolved by the SAME TUI overlay responder (`respond_dialog`'s native branch); native widget/footer/header factories write the host-scoped factory registries and emit the SAME registration/removal notifications as the JS prelude; terminal input stays on the TUI boundary |
//! | [`ProviderService`] | the session's `Arc<ProviderRegistry>` (physical model, auth environment), the `mcp` builtin's shared registry, and the process-wide native provider-auth hook table (login/refresh/logout seam; registrations are last-wins, bounded, and removed on this binding's teardown) |
//! | [`RendererService`] | host-owned native renderer registries (message/entry by custom type, markdown transformers in registration order, tool resolver chains + fallback); panic-isolated and exposed to the TUI through the bound context |
//! | [`CompletionService`] | host-owned native completion registries (per-command argument providers, global autocomplete providers); `suggest` merges/dedupes in provider order and isolates panics |
//! | [`FlagService`] | the JS runtime's flag store (CLI value > default) or the unified registry's native `FlagDef`s |
//! | [`StatusService`] | a process-local live cell; [`PluginHost::statuses`](crate::plugins::PluginHost::statuses) merges it |
//! | [`EventService`] | a bounded synchronous `pi.events` bus |
//!
//! The two renderer/completion services are **host-scoped**: the registries
//! live in a weak per-`PluginHost` cell owned by this module, so rebinding the
//! same host (`bind_context` is the documented invalidation point) reuses the
//! plugin registrations instead of recreating them, and the TUI reads the
//! same services through `PluginHost::context()`. Native widget/header/footer
//! factories are written by [`LiveUiService`] into the same host-scoped cell
//! and resolved by [`NativeRendererService`]: one `FactoryWidget` cache in the
//! TUI, one refresh path, no second cache. Native factories win at a slot the
//! JS face also registered; removing the native registration reveals the JS
//! one (see `tui::renderers`).
//! `ProviderService` is read-only
//! here: the registration mutators stay default no-ops because the session
//! host applies provider/virtual-model definitions from the unified plugin
//! registry at the next rebuild. On [`SessionService`], every mutator with a
//! real backing is live: `abort` and `set_active_tools` write synchronously,
//! while `set_model`, `set_thinking_level`, `set_session_name`, `set_label`,
//! `compact` and `shutdown` schedule the existing async session operation on
//! the caller's tokio runtime (never blocking the caller). `wait_for_idle`
//! polls the run claim on a 25 ms tick. `new_session`, `fork`,
//! `navigate_tree` and `switch_session` keep the trait defaults:
//! they need UI/session replacement (or return values) that this trait cannot
//! express — the missing seam, named here so no adapter fakes them.
//! `reload` IS bound: it schedules [`AgentSession::reload`], which reloads
//! extensions, native plugins, tools, skills, prompts and themes in place
//! without moving the session (a refusal while a run is in flight is logged).
//!
//! # Native action queue
//!
//! `MessageService` is synchronous by contract (the JS face enqueues and
//! returns). A native call therefore builds the exact JSON payload
//! `sendMessageJson` / `sendUserMessageJson` / `appendEntryJson` receive,
//! queues it in [`AgentSession`]'s bounded
//! [`NATIVE_ACTION_QUEUE_CAP`](crate::agent_session::NATIVE_ACTION_QUEUE_CAP)
//! FIFO, and schedules `flush_native_actions` on the caller's runtime (no
//! runtime: the action waits for the next drain). The flush is also called
//! from `apply_extension_notifications`, so run boundaries and idle drains
//! apply native and JS actions through the same handlers and produce identical
//! persisted entries.
//!
//! # Native dialogs
//!
//! `UiService::select/confirm/input/editor` ride the SAME interactive dialog
//! channel as the JS `ctx.ui.*` methods:
//!
//! 1. the call builds the exact `{title, ...}` options payload the JS prelude
//!    spreads into `__pir_host.uiDialog`, and allocates an id from the host's
//!    [`NativeDialogCell`] (top-half `u64` range; JS ids are the low sequential
//!    range, so the two can never collide);
//! 2. it pushes ONE [`Notification::DialogRequest`] with those fields and that
//!    id onto the JS runtime's presentation queue — the queue the TUI drains
//!    and promotes in arrival order;
//! 3. it awaits the cell's one-shot reply. The TUI's existing dialog responder
//!    routes a native id to [`NativeDialogCell::respond`] instead of the JS
//!    `respond_dialog` channel, so submit, Esc/`Ctrl+C` cancel and the overlay
//!    timeout all resolve the native future through the same result mapping.
//!
//! The renderer re-entrancy rule is shared too: the check runs synchronously
//! at call time against the runtime's `has_ui`, the exact value the
//! `RendererUiGuard` clears while a renderer/factory callback owns the UI
//! thread. A dialog requested in that window resolves `None` immediately,
//! like the JS host fn's `null`, instead of parking a request only the blocked
//! thread could drain. A session configured without a UI sink keeps failing
//! closed with [`PluginServiceError::capability`].
//!
//! # Native provider auth hooks
//!
//! `ProviderService::register_auth_hooks` installs a typed
//! [`ProviderAuthHooks`] implementation for one provider id into the
//! process-wide [`provider_auth_hook_registry`]. The auth entry points
//! (`ai::auth::login_and_store`, `refresh_stored_oauth_credential`,
//! `cli::logout`) consult that table first:
//!
//! - **Precedence:** a native hook wins over the builtin flow and over a JS
//!   `providerConfig.oauth` callback (`JsOAuthFlow`) for the same provider.
//!   Exactly one login flow runs; the fallback runs only when no hook is
//!   registered or the hook reports `ProviderAuthError::Unavailable`.
//! - **Credential writes:** native login/refresh results are persisted through
//!   the same `FileAuthStorage` (`auth.json`) read-modify-write path as
//!   builtins, as `StoredCredential::ApiKey` or `StoredCredential::OAuth`.
//!   `Failed`/`Cancelled` return before the write, so the previous credential
//!   remains in place.
//! - **Ownership:** the table is bounded (`MAX_PROVIDER_AUTH_HOOKS`, last-wins
//!   per provider, insertion-order eviction) and stores only the hook `Arc` —
//!   never this session or its `PluginHost`. Dropping this service removes the
//!   registrations it still owns, and
//!   `ProviderService::unregister_auth_hooks` removes one explicitly.
//!
//! Unsupported flows and their exact reasons are documented on
//! [`ProviderAuthHooks`](crate::plugins::services::ProviderAuthHooks): custom
//! component/browser UI has no shared TUI ABI, host-owned device-code polling
//! does not exist (the hook must poll itself), and the TUI `/login` provider
//! selector enumerates the static OAuth provider list, so a hook-only provider
//! is reachable through `pir auth login <id>` but is not listed there.
//!
//! # Remaining capabilities: exact host seams
//!
//! These capabilities are either bound below or need one of these seams
//! before a native adapter can be honest.
//!
//! - `UiService::on_terminal_input` is bound: the handler joins the host's
//!   registration-order registry (`PluginHost::on_terminal_input`), which the
//!   TUI's raw-input producer consults on the same `terminal_input` dispatch
//!   as the JS face (`PluginHost::dispatch_terminal_input`).
//! - Message/entry/markdown/tool native resolution IS bound here; the TUI
//!   reads the service from the host context (`PluginHost::context()`) and
//!   invokes it at the same seam as the JS renderer bridge (see
//!   `tui::renderers` for the precedence table).
//! - Native completion providers ARE bound here; the TUI's extension
//!   dispatcher merges them with the JS chain (`tui::autocomplete`).
//! - `SessionService::reload` is bound to [`AgentSession::reload`]
//!   (fire-and-forget on the caller's runtime). `SessionService::new_session`
//!   / `fork` / `navigate_tree` / `switch_session` stay documented no-ops by
//!   ABI contract: the trait returns `()` (no verdict) and carries no
//!   cancellation channel. The honest native face is
//!   [`crate::agent_session::NativeSessionHost`] (resolved from a
//!   `PluginContext` with
//!   [`native_session_host`](crate::agent_session::native_session_host)):
//!   typed [`SessionReplacement`](crate::agent_session::SessionReplacement)
//!   results, caller `AbortSignal` cancellation, and an explicit
//!   `capability()` probe that reports `Unsupported` instead of a silent
//!   success. The JS command ctx keeps its own working path
//!   (`ctx.newSession` / `fork` / `navigateTree` / `switchSession`).
//! - Native floating `ctx.ui.custom` carries `overlayOptions`: a factory
//!   wrapped with
//!   [`NativeOverlayFactory`](crate::agent_session::NativeOverlayFactory)
//!   hands its typed options to this service, which emits the exact
//!   `overlay: true` + `overlayOptions` wire the TUI already parses.
//!   `NativeSessionHost::custom_overlay` is the explicit
//!   result-returning/capability-checked variant; see
//!   [`crate::extensions::overlay`].
//!
//! The [`EventService`] bus is native-only: the QuickJS `pi.events` face is
//! not bridged here (the trait contract leaves cross-face delivery to the
//! host), and plugin lifecycle events never ride it — those stay on the one
//! ordered [`AgentPlugin::on_event`](crate::plugins::AgentPlugin::on_event)
//! dispatch.
//!
//! # Zero-copy / ABI guarantees
//!
//! - `cwd` is one shared `Arc<Path>`; every read is an `Arc` clone.
//! - `scoped_models` moves the session's `Vec<ScopedModel>` into an
//!   `Arc<[ScopedModel]>` (one allocation, never JSON).
//! - `commands`, `all_tools` and `active_tools` are built from the live typed
//!   `CommandDef` / `ToolDef` / `AgentTool` values; no `JsonValue` is parsed
//!   on the read path. `commands`/`all_tools` memoize the materialized
//!   `Arc<[CommandInfo]>`/`Arc<[ToolInfo]>` per binding and invalidate it on
//!   the host's registry generation, so a repeated read is one `u64` compare
//!   plus an `Arc` clone (the schema-building `parameters()` pass runs once
//!   per generation, not once per read).
//! - `model` is the session's typed [`Model`] clone wrapped in one `Arc` (the
//!   session stores `RwLock<Model>`, not `Arc<Model>`); no serialization.
//! - `thinking_level` is the typed enum.
//! - `physical_model` returns the registry's typed [`Model`].
//! - `FlagService` returns the typed [`FlagValue`].
//! - status cells are `Arc<str>` pairs.
//! - JSON crosses only where the data is inherently JSON: the MCP server
//!   configs (`RegisteredMcpServer::config`) and the event bus payload.
//!
//! `snapshot()` / `entries()` return the `Arc`s the plugin host retains for
//! the last pushed [`SessionContextSnapshot`]: the host wraps each push in one
//! `Arc` and stores the snapshot's `entries` field as a second `Arc`, so a
//! repeated read is one weak-host upgrade plus an `Arc` clone — never a
//! re-serialization. The snapshot value is cloned exactly once per push, and
//! only when a JS runtime consumes it; a Rust-only host never clones.
//!
//! # Lifetime / cycle rules
//!
//! Adapters hold `Arc`s to the small session-owned cells (provider registry,
//! tool registry, atomic state cells, override cell) and a `Weak` reference to
//! the [`AgentSession`] only for the values that have no shareable cell
//! (`session_id`, `session_path`, `model`, `thinking`, scoped models, system
//! prompt). The plugin host is held as a `Weak` too, so the
//! `host -> context -> services -> host` chain never forms a cycle.
//!
//! The weak session slot is filled by
//! [`AgentSession::new_arc_with_plugins`](crate::agent_session::AgentSession::new_arc_with_plugins)
//! after the `Arc` exists and before [`PluginHost::bind_context`] runs, so the
//! very first `on_context` already sees a live session. Constructors returning
//! `Self` (`new`/`new_with_plugins`) still bind a context (status, events,
//! flags, providers, cwd, mode, idle/pending are live), but the
//! session-identity reads report their honest absence until the caller uses
//! the `Arc` constructor — a by-value constructor cannot hand out a `Weak` to
//! an `Arc` that does not exist yet.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use crate::agent::nested_tool_calls::{NestedShared, NestedToolExecutor, NestedToolOutcome};
use crate::agent::tool_batch::ToolResult as BatchToolResult;
use crate::agent::tool_execution::{ToolContext, ToolExposure, ToolRegistry, ToolSessionEnv};
use crate::agent_session::{AgentSession, NativeActionKind, SESSION_RUNNING};
use crate::ai::provider::ProviderRegistry;
use crate::ai::types::{Model, ThinkingLevel};
use crate::extensions::{
    ExtensionHandle, FlagValue, NativeOverlayHandle, NativeOverlayOptions, Notification,
    RendererCallOutcome, RendererOutcome, SessionContextSnapshot,
};
use crate::plugins::host::NativeDialogCell;
use crate::plugins::plugin::services::{
    provider_auth_hook_registry, ArgumentCompletions, AutocompleteItem, AutocompleteProvider,
    BarFactory, CancellationHandle, CommandInfo, CompletionContext, CompletionService,
    CustomMessage, DialogOptions, EntryRenderer, EventCallback, EventService, ExecOptions,
    ExecResult, ExecuteToolOptions, ExtensionMode, FactoryRenderContext, FlagService,
    MarkdownTransformer, MessageRenderer, MessageService, NotifyLevel, OverlayComponent,
    OverlayComponentFactory, OverlayDone, PluginServiceError, PluginServices, ProviderAuthHooks,
    ProviderService, RegisteredMcpServer, RendererKind, RendererService, ScopedModel, SelectOption,
    SendMessageOptions, SendUserMessageOptions, ServiceFuture, ServiceResult, SessionService,
    StatusEntry, StatusService, StatusSnapshot, Subscription, SystemPromptOptions, ThemeInfo,
    ThemeSetResult, ThemeSnapshot, ToolCallRef, ToolInfo, ToolRenderRequest, ToolRendererResolver,
    ToolService, UiService, UserContent, WidgetFactory, WidgetPlacement, WorkingIndicator,
    CUSTOM_OVERLAY_EVENT_KEY, FACTORY_RENDER_REQUEST_KEY,
};
use crate::plugins::plugin::PluginContext;
use crate::plugins::PluginHost;

/// Upper bound on subscribers per event-bus channel. `subscribe` beyond the
/// bound returns a no-op [`Subscription`] instead of growing the fan-out
/// without limit; an `emit` therefore costs at most this many callbacks.
const MAX_EVENT_SUBSCRIBERS_PER_CHANNEL: usize = 256;

/// How long the provider-auth oracle reuses one credential-environment read.
/// `has_auth` can be polled per model row; `load_pi_environment` reads settings
/// and `models.json` (and `has_configured_auth` may read `auth.json`), so a
/// short bounded-staleness cache keeps the read path cheap while still
/// observing a `/login` on the next refresh.
///
/// Refresh discipline: the first call (no cached answer yet) may block on the
/// read; every later call is served from the snapshot. Once the TTL expires,
/// the caller gets the stale snapshot immediately and at most one background
/// task performs the read. A refresh in flight suppresses further reads until
/// it completes, so N hot-path calls cost one background read per TTL window.
const AUTH_CACHE_TTL: Duration = Duration::from_secs(1);

/// Safety-net re-check for [`LiveSessionService::wait_for_idle`]. The wait is
/// event-driven (`idle_notify` fires when the run claim releases), so a waiter
/// normally costs ZERO wake-ups while the session is busy; this coarse timer
/// only keeps a session dropped without a claim release from parking the
/// waiter forever.
const IDLE_WAIT_FALLBACK: Duration = Duration::from_secs(5);

/// `commands()` memo: `(host registry generation, materialized commands)`.
/// `None` in the tuple slot means the host is gone (the typed set is empty).
type CommandsCache = Mutex<Option<(Option<u64>, Arc<[CommandInfo]>)>>;

/// `all_tools()` memo: `(host registry generation, materialized tool defs)`.
type ToolsCache = Mutex<Option<(Option<u64>, Arc<[ToolInfo]>)>>;

/// One `pi.events` subscriber: `(subscription id, callback)`.
type EventSubscriber = (u64, EventCallback);

/// The `pi.events` channel map: channel name → subscribers in subscription
/// order.
type EventChannels = Arc<Mutex<HashMap<String, Vec<EventSubscriber>>>>;

// ---------------------------------------------------------------------------
// Binding
// ---------------------------------------------------------------------------

/// One session's live service set plus the deferred session handle.
pub(super) struct LiveSessionServices {
    state: Arc<LiveSessionState>,
    services: PluginServices,
}

impl LiveSessionServices {
    /// Point the service adapters at the session `Arc` (called before the
    /// context is bound, so `on_context` observes a live session).
    pub(super) fn bind_session(&self, session: &Arc<AgentSession>) {
        *self
            .state
            .session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(session));
    }

    /// Finish the context. The caller hands this to `PluginHost::bind_context`.
    pub(super) fn into_context(self) -> PluginContext {
        self.services.build()
    }
}

/// Build the live service set for `session`/`host`. Every captured field is a
/// small `Arc` clone (or a `Weak`); the session itself is not kept alive.
pub(super) fn live_services(session: &AgentSession, host: &Arc<PluginHost>) -> LiveSessionServices {
    let config = session.extensions.as_ref().map(ExtensionHandle::config);
    // The native renderer/completion registries are HOST-owned: keyed by the
    // `PluginHost` address and held weakly process-wide, so a rebind of the
    // same host reuses them (a plugin's `on_context` re-registration is
    // idempotent under the first-wins / compose rules) while the session and
    // its host can still drop without a cycle or a leaked strong reference.
    let native = shared_native_registries(host);
    let state = Arc::new(LiveSessionState {
        cwd: Arc::from(session.cwd.as_path()),
        registry: Arc::clone(&session.registry),
        tools: Arc::clone(&session.tools),
        session_state: Arc::clone(&session.session_state),
        idle_notify: Arc::clone(&session.idle_notify),
        pending_messages: Arc::clone(&session.pending_messages),
        active_tools_override: Arc::clone(&session.active_tools_override),
        tool_selection: session
            .tool_selection
            .as_ref()
            .map(|selection| selection.enabled.clone()),
        uses_default_tools: session.uses_default_tools(),
        default_tool_exclusions: Arc::from(
            session
                .default_tool_exclusions()
                .into_iter()
                .map(Arc::<str>::from)
                .collect::<Vec<_>>(),
        ),
        context_derived_prompt: session.context_derived_prompt,
        mode: config
            .as_ref()
            .map(|config| ExtensionMode::from_mode(&config.mode))
            .unwrap_or_default(),
        has_ui: config.as_ref().is_some_and(|config| config.has_ui),
        // The SAME resolved value the run gate used
        // (`runtime_loader::load_settings_for_in`): operator override, then
        // the extension `project_trust` decision recorded/pinned by the
        // startup host, then the persisted store / `defaultProjectTrust`
        // policy. The session's build-time agent directory is used so a
        // mid-process `PIR_AGENT_DIR` change cannot split this read from the
        // settings the session loaded.
        project_trusted: crate::extensions::project_trust::resolve_project_trust(
            &session.cwd,
            &session.agent_dir,
        ),
        js: session.extensions.clone(),
        host: Arc::downgrade(host),
        factories: Arc::clone(&native),
        status: Arc::new(NativeStatusService::default()),
        events: Arc::new(NativeEventBus::default()),
        session: RwLock::new(None),
        auth: Arc::new(Mutex::new(AuthCache::default())),
        commands_cache: Mutex::new(None),
        tools_cache: Mutex::new(None),
    });
    let mut services = PluginServices::new()
        .with_session(Arc::new(LiveSessionService {
            state: Arc::clone(&state),
        }))
        .with_providers(Arc::new(LiveProviderService {
            state: Arc::clone(&state),
            registered_auth_hooks: Mutex::new(HashMap::new()),
        }))
        .with_events(state.events.clone())
        .with_status(state.status.clone())
        .with_flags(Arc::new(LiveFlagService {
            state: Arc::clone(&state),
        }))
        // Message/Tool services share the session's live cells and enqueue
        // through the exact JS wire payloads (see the module docs).
        .with_messages(Arc::new(LiveMessageService {
            state: Arc::clone(&state),
        }))
        .with_tools(Arc::new(LiveToolService {
            state: Arc::clone(&state),
            next_call: AtomicU64::new(0),
        }))
        .with_renderers(Arc::new(NativeRendererService {
            registries: Arc::clone(&native),
            js: state.js.clone(),
        }))
        .with_completions(Arc::new(NativeCompletionService { registries: native }));
    // UI service: bound only when a presentation sink exists — the JS
    // runtime's bounded notification queue, which is the one queue the TUI
    // drain, the headless native mirror, and `PluginHost::take_notifications`
    // share. Without a runtime handle there is no queue to write and no
    // projection to mirror, so the accessor stays `None` (dialogs and every
    // other UI capability fail closed).
    if state.js.is_some() {
        services = services.with_ui(Arc::new(LiveUiService {
            state: Arc::clone(&state),
        }));
    }
    LiveSessionServices { state, services }
}

/// The small, shareable session state every adapter reads.
struct LiveSessionState {
    cwd: Arc<Path>,
    registry: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    /// Run claim (`SESSION_IDLE` / `SESSION_RUNNING` / compacting).
    session_state: Arc<AtomicU8>,
    /// The session's claim-release signal (the same `Notify` its own
    /// `wait_until_idle` uses): `wait_for_idle` awaits it instead of polling.
    idle_notify: Arc<Notify>,
    /// Queued steering/follow-up count.
    pending_messages: Arc<AtomicUsize>,
    /// `setActiveTools` override written by tools/extensions.
    active_tools_override: Arc<RwLock<Option<Vec<String>>>>,
    /// Host-resolved tool selection (the lowest-precedence active override).
    tool_selection: Option<Vec<String>>,
    /// Whether that selection is the DEFAULT-tools derivation
    /// ([`AgentSession::uses_default_tools`]): an explicit `--tools` /
    /// `--no-tools` selection is authoritative and plugins must not widen the
    /// active set past it. Immutable for the session's lifetime, so it is
    /// copied once here instead of re-read through the weak session handle.
    uses_default_tools: bool,
    /// `-name` exclusions of a default-tools selection (empty for an explicit
    /// one). Read by the `mcp` builtin before it re-activates a registered
    /// bridge: an explicit exclusion outranks registration-time activation.
    default_tool_exclusions: Arc<[Arc<str>]>,
    /// Whether the system prompt came from the context builder (so the stored
    /// prompt is not a caller-supplied custom prompt).
    context_derived_prompt: bool,
    mode: ExtensionMode,
    has_ui: bool,
    project_trusted: bool,
    /// The JS runtime handle, for `pi.getFlag` (CLI value > default).
    js: Option<ExtensionHandle>,
    host: Weak<PluginHost>,
    /// The host-scoped native registries shared with [`NativeRendererService`]
    /// and [`NativeCompletionService`]. The UI service writes the factory
    /// slots here; the renderer service resolves them for the TUI.
    factories: Arc<NativeRegistries>,
    status: Arc<NativeStatusService>,
    events: Arc<NativeEventBus>,
    /// Filled by [`LiveSessionServices::bind_session`] on the `Arc`
    /// construction path only.
    session: RwLock<Option<Weak<AgentSession>>>,
    /// Bounded-staleness credential-environment snapshot. Shared with the
    /// background refresh task, which is why the cell itself is an `Arc`.
    auth: Arc<Mutex<AuthCache>>,
    /// `commands()` memo keyed on the host registry generation (`None` = the
    /// host is gone, so the typed command set is empty).
    commands_cache: CommandsCache,
    /// `all_tools()` memo keyed on the host registry generation (`None` = no
    /// host). The core tool set is immutable for the session's lifetime.
    tools_cache: ToolsCache,
}

/// Credential-environment snapshot plus the single-flight refresh flag.
#[derive(Default)]
struct AuthCache {
    /// Last completed read `(completed_at, authenticated provider ids)`.
    snapshot: Option<(Instant, Arc<[Arc<str>]>)>,
    /// True while a cold read or a background refresh owns the disk read.
    refreshing: bool,
}

impl LiveSessionState {
    fn session(&self) -> Option<Arc<AgentSession>> {
        self.session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    fn host(&self) -> Option<Arc<PluginHost>> {
        self.host.upgrade()
    }

    /// The live active-tool override, in the session's documented precedence
    /// order (`setActiveTools` cell, plugin-host override, `--tools`
    /// selection). `None` means the exposure defaults decide.
    ///
    /// Applies the same direct-MCP-bridge union as the session's own
    /// [`AgentSession::resolve_active_override`]
    /// ([`mcp_direct_tool_names_for`]): the `mcp` builtin reads this value
    /// when it recomputes the active set, so a discrepancy here would make the
    /// recompute write a set the loadout then re-expands differently.
    fn active_override(&self) -> Option<Vec<String>> {
        // `mut` is used only by the `mcp` direct-bridge union below; without
        // that feature the binding is read-only. Keeping the cfg block (rather
        // than two bindings) keeps the precedence order single-sourced.
        #[allow(unused_mut)]
        let mut names = self
            .active_tools_override
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .or_else(|| self.host().and_then(|host| host.active_tools_override()))
            .or_else(|| self.tool_selection.clone())?;
        #[cfg(feature = "mcp")]
        {
            let exclusions: Vec<String> = self
                .default_tool_exclusions
                .iter()
                .map(|name| name.to_string())
                .collect();
            let host = self.host();
            for name in crate::agent_session::tools::mcp_direct_tool_names_for(
                self.uses_default_tools,
                &exclusions,
                host.as_deref(),
            ) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        Some(names)
    }

    /// The typed tool definitions of the session's merged callable surface:
    /// core tools first, then the unified plugin registry's resolved defs
    /// (first name wins). Memoized per host registry generation, so both
    /// [`SessionService::all_tools`] and [`ToolService::callable_tools`] share
    /// ONE materialization — the schema-building `parameters()` pass runs once
    /// per generation, not once per read and not once per service.
    fn all_tools(&self) -> Arc<[ToolInfo]> {
        let host = self.host();
        let generation = host.as_ref().map(|host| host.registry_generation());
        if let Some((cached_generation, defs)) = self
            .tools_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            if *cached_generation == generation {
                return Arc::clone(defs);
            }
        }
        // Core tools first, then the unified plugin registry's resolved defs
        // (first name wins) — the same precedence the session context push
        // applies for the JS facades. Only typed definitions are produced, and
        // the schema-building `parameters()` pass runs once per generation.
        let mut defs: Vec<ToolInfo> = crate::coding_agent::tools::tool_definitions(&self.tools)
            .into_iter()
            .map(|tool| ToolInfo::core(tool.name, tool.description, tool.parameters))
            .collect();
        if let Some(host) = &host {
            let mut seen: BTreeSet<String> = defs.iter().map(|def| def.name.clone()).collect();
            for def in host.tool_defs() {
                if seen.insert(def.name.clone()) {
                    defs.push(def);
                }
            }
        }
        let defs: Arc<[ToolInfo]> = Arc::from(defs);
        *self
            .tools_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((generation, Arc::clone(&defs)));
        defs
    }
}

// ---------------------------------------------------------------------------
// SessionService
// ---------------------------------------------------------------------------

struct LiveSessionService {
    state: Arc<LiveSessionState>,
}

impl LiveSessionService {
    /// Run one session operation on the caller's tokio runtime without
    /// blocking the caller.
    ///
    /// Returns `false` (and schedules nothing) when the weak session no longer
    /// upgrades or no runtime is current — never panics. A panic inside the
    /// spawned task is contained by tokio (`JoinError` on the discarded
    /// handle), so it cannot unwind into the plugin that made the call.
    fn schedule<F, Fut>(&self, work: F) -> bool
    where
        F: FnOnce(Arc<AgentSession>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some(session) = self.state.session() else {
            return false;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        // Detached by design: the service method is synchronous. Dropping the
        // join handle is the documented "fire and forget" form.
        drop(runtime.spawn(work(session)));
        true
    }
}

impl SessionService for LiveSessionService {
    fn snapshot(&self) -> Option<Arc<SessionContextSnapshot>> {
        // The host retains the exact `Arc` it pushed: repeated reads are one
        // weak upgrade plus a refcount bump, never a rebuild/re-serialization.
        self.state.host()?.session_context_snapshot()
    }

    fn entries(&self) -> Option<Arc<JsonValue>> {
        // The `entries` field of the retained snapshot, as its own `Arc` (one
        // clone per push), so this read does not deep-clone JSON either.
        self.state.host()?.session_entries_snapshot()
    }

    fn session_id(&self) -> Option<Arc<str>> {
        self.state
            .session()
            .and_then(|session| session.session_id())
            .map(Arc::from)
    }

    fn session_path(&self) -> Option<Arc<Path>> {
        self.state
            .session()
            .and_then(|session| session.session_path())
            .map(|path| Arc::from(path.as_path()))
    }

    fn cwd(&self) -> Option<Arc<Path>> {
        // One shared `Arc<Path>` for the whole binding: an `Arc` clone per
        // read, no allocation and no string conversion.
        Some(Arc::clone(&self.state.cwd))
    }

    fn mode(&self) -> ExtensionMode {
        self.state.mode.clone()
    }

    fn has_ui(&self) -> bool {
        self.state.has_ui
    }

    fn is_idle(&self) -> bool {
        // Upstream `isIdle` = "not streaming", read from the same live cell
        // the run claims.
        self.state.session_state.load(Ordering::SeqCst) != SESSION_RUNNING
    }

    fn is_project_trusted(&self) -> bool {
        self.state.project_trusted
    }

    /// `ctx.reload()` (upstream `ExtensionCommandContext.reload`): schedule
    /// the session's resource reload (extensions, native plugins, tools,
    /// skills, prompts, themes) on the caller's runtime.
    ///
    /// Fire-and-forget by contract (the trait returns no result, the same
    /// convention `compact`/`shutdown` follow). A refusal — a run or
    /// compaction in flight — is logged, never silently dropped; the next
    /// settle drain still applies a JS-face reload recorded as an action.
    fn reload(&self) {
        self.schedule(|session| async move {
            if let Err(error) = session.reload().await {
                crate::ext_diag!("[plugins] native ctx.reload() failed: {error}");
            }
        });
    }

    fn has_pending_messages(&self) -> bool {
        self.state.pending_messages.load(Ordering::SeqCst) > 0
    }

    fn model(&self) -> Option<Arc<Model>> {
        // The session stores `RwLock<Model>`; the typed clone is wrapped in
        // one `Arc` for the plugin boundary (no JSON hop).
        self.state
            .session()
            .map(|session| Arc::new(session.current_model()))
    }

    fn scoped_models(&self) -> Arc<[ScopedModel]> {
        match self.state.session() {
            // The session's `Vec<ScopedModel>` moves into the shared slice.
            Some(session) => Arc::from(session.scoped_models()),
            None => Arc::from(Vec::new()),
        }
    }

    fn thinking_level(&self) -> ThinkingLevel {
        self.state
            .session()
            .map(|session| session.current_thinking())
            .unwrap_or(ThinkingLevel::Off)
    }

    fn system_prompt(&self) -> Arc<str> {
        self.state
            .session()
            .map(|session| {
                Arc::from(
                    session
                        .system_prompt_snapshot()
                        .unwrap_or_default()
                        .as_str(),
                )
            })
            .unwrap_or_else(|| Arc::from(""))
    }

    fn system_prompt_options(&self) -> SystemPromptOptions {
        let custom_prompt = if self.state.context_derived_prompt {
            None
        } else {
            self.state
                .session()
                .and_then(|session| session.system_prompt_snapshot())
                .map(Arc::from)
        };
        SystemPromptOptions {
            cwd: Some(Arc::clone(&self.state.cwd)),
            custom_prompt,
        }
    }

    fn commands(&self) -> Arc<[CommandInfo]> {
        let host = self.state.host();
        // `registry_generation` is bumped after a rebuild publishes its
        // snapshot (Release/Acquire), so a cache entry stamped with a
        // generation always describes that generation's registry (a read
        // racing a rebuild either hits the old pair or misses on the new one).
        let generation = host.as_ref().map(|host| host.registry_generation());
        if let Some((cached_generation, commands)) = self
            .state
            .commands_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            if *cached_generation == generation {
                return Arc::clone(commands);
            }
        }
        let commands: Arc<[CommandInfo]> = match &host {
            // Typed `CommandDef`s straight from the live unified registry (the
            // same rows the JS `getCommands()` facade receives).
            Some(host) => Arc::from(
                host.registry()
                    .resolved_commands()
                    .into_iter()
                    .map(|command| command.def)
                    .collect::<Vec<_>>(),
            ),
            None => Arc::from(Vec::new()),
        };
        *self
            .state
            .commands_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((generation, Arc::clone(&commands)));
        commands
    }

    fn all_tools(&self) -> Arc<[ToolInfo]> {
        self.state.all_tools()
    }

    fn uses_default_tools(&self) -> bool {
        self.state.uses_default_tools
    }

    fn default_tool_exclusions(&self) -> Arc<[Arc<str>]> {
        Arc::clone(&self.state.default_tool_exclusions)
    }

    fn active_tools(&self) -> Arc<[Arc<str>]> {
        let override_names = self.state.active_override();
        // With the live session `Arc` the merged-registry cache answers with
        // the exact loadout surface. The by-value construction paths have no
        // session handle, but they still have the host: derive the same set
        // from `all_tools()` (core definitions first, then the host registry)
        // instead of the base registry alone — the base registry would drop
        // every default-active plugin tool as soon as an override (for
        // example the MCP discovery activation) is written.
        let names: Vec<String> = match (self.state.session(), self.state.host()) {
            (Some(session), Some(host)) => {
                let registry = session.merged_tool_registry(&host);
                registry.active_names(override_names.as_deref())
            }
            _ => {
                let defs = self.state.all_tools();
                let exposure = |def: &ToolInfo| {
                    ToolExposure::parse(&def.exposure).unwrap_or(ToolExposure::Direct)
                };
                match override_names.as_deref() {
                    Some(wanted) => {
                        let by_name: std::collections::BTreeMap<&str, &ToolInfo> =
                            defs.iter().map(|def| (def.name.as_str(), def)).collect();
                        let mut seen = std::collections::BTreeSet::new();
                        wanted
                            .iter()
                            .filter(|name| seen.insert((*name).clone()))
                            .filter_map(|name| by_name.get(name.as_str()).copied())
                            .filter(|def| exposure(def) != ToolExposure::Hidden)
                            .map(|def| def.name.clone())
                            .collect()
                    }
                    None => {
                        let mut names: Vec<String> = defs
                            .iter()
                            .filter(|def| exposure(def).active_on_registration(def.default_active))
                            .map(|def| def.name.clone())
                            .collect();
                        names.sort();
                        names
                    }
                }
            }
        };
        Arc::from(names.into_iter().map(Arc::<str>::from).collect::<Vec<_>>())
    }

    // -- writes / scheduled actions -----------------------------------------

    fn abort(&self) {
        // Synchronous by contract: `AgentSession::abort` only fires the run
        // signal and the per-call latches. A dropped session is a no-op.
        if let Some(session) = self.state.session() {
            session.abort();
        }
    }

    fn set_active_tools(&self, names: &[Arc<str>]) {
        // Write the session's live override cell (the adapter already holds
        // it), so the next tool-loadout rebuild observes the new set. An empty
        // slice stays `Some(empty)` — "no active tools" — never `None`
        // ("defaults").
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        *self
            .state
            .active_tools_override
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(names);
    }

    fn set_model(&self, model: &Model) -> bool {
        let model = model.clone();
        self.schedule(move |session| async move {
            let _ = session.set_model(model).await;
        })
    }

    fn set_thinking_level(&self, level: ThinkingLevel) {
        let _ = self.schedule(move |session| async move {
            let _ = session.set_thinking_level(level).await;
        });
    }

    fn set_session_name(&self, name: Option<&str>) {
        let name = name.map(str::to_owned);
        let _ = self.schedule(move |session| async move {
            match name {
                Some(name) => {
                    let _ = session.set_session_name(&name).await;
                }
                // Clearing persists an empty `session_info` entry (upstream);
                // with no storage the name cell is the only store.
                None => match &session.session {
                    Some(manager) => {
                        let mut manager = manager.lock().await;
                        let _ = manager.set_session_name(None);
                    }
                    None => {
                        *session
                            .session_name
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    }
                },
            }
        });
    }

    fn set_label(&self, entry_id: &str, label: Option<&str>) {
        let entry_id = entry_id.to_owned();
        let label = label.map(str::to_owned);
        let _ = self.schedule(move |session| async move {
            // Labels live on the persisted session manager; an ephemeral
            // session has no entry to attach one to (quiet no-op).
            let Some(manager) = &session.session else {
                return;
            };
            let mut manager = manager.lock().await;
            let _ = manager.set_label(&entry_id, label.as_deref());
        });
    }

    fn compact(&self, custom_instructions: Option<&str>) {
        let instructions = custom_instructions.map(str::to_owned);
        let _ = self.schedule(move |session| async move {
            // The compaction events report the outcome; the trait has no
            // result slot for it.
            let _ = session.compact(instructions.as_deref()).await;
        });
    }

    fn shutdown(&self) {
        let _ = self.schedule(|session| async move {
            session.shutdown().await;
        });
    }

    fn wait_for_idle(&self) -> ServiceFuture<'static, ()> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            loop {
                // A dropped session ends the wait instead of spinning on its
                // last claim state.
                if state.session().is_none()
                    || state.session_state.load(Ordering::SeqCst) != SESSION_RUNNING
                {
                    return;
                }
                wait_for_claim_change(&state.session_state, &state.idle_notify).await;
            }
        })
    }
}

/// Wait for ONE run-claim change, event-driven.
///
/// The run claim's release notifies `idle_notify` (`SessionStreamingGuard` /
/// the manual-compaction claim), so a busy session costs ZERO wake-ups; the
/// coarse `IDLE_WAIT_FALLBACK` timer is a liveness net for a session dropped
/// without a release, never a poll loop. The notification is registered before
/// the claim is re-read, so a release racing the registration cannot be
/// missed.
async fn wait_for_claim_change(state: &AtomicU8, notify: &Notify) {
    if state.load(Ordering::SeqCst) != SESSION_RUNNING {
        return;
    }
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if state.load(Ordering::SeqCst) != SESSION_RUNNING {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            // Create the fallback timer while entered in the runtime (the
            // constructor needs the timer context); awaiting it afterwards
            // only needs its waker.
            let fallback = {
                let _runtime = handle.enter();
                tokio::time::sleep(IDLE_WAIT_FALLBACK)
            };
            tokio::select! {
                _ = notified => {}
                _ = fallback => {}
            }
        }
        // No timer context: `Notify` is executor-agnostic, so the wait still
        // works without a runtime timer or a busy spin.
        Err(_) => notified.await,
    }
}

/// Test seam: observe one run-claim wait (integration tests live in `tests/`;
/// inline test modules are forbidden in `src/`).
#[doc(hidden)]
pub async fn wait_for_claim_change_for_tests(state: &AtomicU8, notify: &Notify) {
    wait_for_claim_change(state, notify).await
}

// ---------------------------------------------------------------------------
// MessageService
// ---------------------------------------------------------------------------

/// Live message injection: a native service call becomes the EXACT JSON
/// payload the JS host function produces and lands in the session's bounded
/// native-action queue. `apply_extension_notifications` drains that queue
/// through `consume_send_message` / `consume_send_user_message` /
/// `consume_append_entry`, so both faces share one persistence path.
struct LiveMessageService {
    state: Arc<LiveSessionState>,
}

impl LiveMessageService {
    /// Queue one action, then flush on the caller's runtime when there is one.
    ///
    /// Without a current runtime the action simply stays queued: the next run
    /// boundary or idle `apply_extension_notifications` drain applies it.
    /// Never panics and never blocks the calling plugin.
    fn enqueue(&self, kind: NativeActionKind, payload: JsonValue) {
        let Some(session) = self.state.session() else {
            return;
        };
        session.queue_native_action(kind, payload);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // Detached by design: the service methods are synchronous and match
        // the JS host functions' fire-and-forget contract.
        drop(runtime.spawn(async move { session.flush_native_actions().await }));
    }
}

impl MessageService for LiveMessageService {
    fn send_message(&self, message: CustomMessage, options: SendMessageOptions) {
        // JS wire shape (`prelude.js.txt` sendMessage):
        // `{message: {customType, content, display, details?}, options:
        // {triggerTurn?, deliverAs?}}`. Optional fields are omitted exactly
        // where `JSON.stringify` drops `undefined`.
        let mut message_json = serde_json::Map::new();
        message_json.insert(
            "customType".to_owned(),
            JsonValue::String(message.custom_type),
        );
        message_json.insert("content".to_owned(), message.content);
        message_json.insert("display".to_owned(), JsonValue::Bool(message.display));
        if let Some(details) = message.details {
            message_json.insert("details".to_owned(), details);
        }
        let mut options_json = serde_json::Map::new();
        if let Some(trigger_turn) = options.trigger_turn {
            options_json.insert("triggerTurn".to_owned(), JsonValue::Bool(trigger_turn));
        }
        if let Some(deliver_as) = options.deliver_as {
            options_json.insert(
                "deliverAs".to_owned(),
                JsonValue::String(deliver_as.as_str().to_owned()),
            );
        }
        self.enqueue(
            NativeActionKind::SendMessage,
            serde_json::json!({
                "message": JsonValue::Object(message_json),
                "options": JsonValue::Object(options_json),
            }),
        );
    }

    fn send_user_message(&self, content: UserContent, options: SendUserMessageOptions) {
        // JS wire shape: `{content: string | block[], options: {deliverAs?}}`.
        let content = match content {
            UserContent::Text(text) => JsonValue::String(text),
            UserContent::Blocks(blocks) => JsonValue::Array(blocks),
        };
        let mut options_json = serde_json::Map::new();
        if let Some(deliver_as) = options.deliver_as {
            options_json.insert(
                "deliverAs".to_owned(),
                JsonValue::String(deliver_as.as_str().to_owned()),
            );
        }
        self.enqueue(
            NativeActionKind::SendUserMessage,
            serde_json::json!({
                "content": content,
                "options": JsonValue::Object(options_json),
            }),
        );
    }

    fn append_entry(&self, custom_type: &str, data: Option<JsonValue>) {
        // JS wire shape: `{customType: String(customType), data: data ?? null}`.
        self.enqueue(
            NativeActionKind::AppendEntry,
            serde_json::json!({
                "customType": custom_type,
                "data": data.unwrap_or(JsonValue::Null),
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// ToolService
// ---------------------------------------------------------------------------

/// Live tool execution, exec, and in-flight updates.
struct LiveToolService {
    state: Arc<LiveSessionState>,
    /// Monotonic child-call sequence: `<caller_id>/<n>` ids never repeat
    /// within one activation even when a caller reuses its call id.
    next_call: AtomicU64,
}

impl ToolService for LiveToolService {
    fn callable_tools(&self) -> Arc<[ToolInfo]> {
        // The merged callable surface (core + registry defs), sharing the
        // per-generation memo with `SessionService::all_tools`.
        self.state.all_tools()
    }

    fn execute_tool(
        &self,
        caller_id: &str,
        name: &str,
        args: JsonValue,
        options: ExecuteToolOptions,
    ) -> ServiceFuture<'static, ServiceResult<NestedToolOutcome>> {
        let state = Arc::clone(&self.state);
        let caller_id = caller_id.to_owned();
        let name = name.to_owned();
        let sequence = self.next_call.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let session = state.session().ok_or_else(|| {
                PluginServiceError::failed("tool execution requires a live session")
            })?;
            let tool_call_id = format!("{caller_id}/{sequence}");
            let started = Instant::now();
            let host = state.host();
            // Core/session tools win the merged registry's first-name-wins
            // precedence, so they are never routed to a same-name plugin
            // registration.
            let core_tool = state.tools.get(&name).is_some();
            let plugin_tool = !core_tool
                && host.as_ref().is_some_and(|host| {
                    host.registry()
                        .resolved_tools()
                        .iter()
                        .any(|(def, _)| def.name == name)
                });
            let result = if plugin_tool {
                let host = host
                    .as_ref()
                    .ok_or_else(|| PluginServiceError::failed("plugin tool host is gone"))?;
                // Reuse the host's backend dispatch: Rust plugin tools run
                // natively, JS tools cross into QuickJS, and a failure is the
                // host's typed error. The call carries the session's live
                // ToolContext (not the host's cwd-only synthetic one), so a
                // native plugin tool observes the same abort signal, session
                // environment, nested-call capability, update channel and
                // background flag a model-issued call receives.
                let context =
                    session.native_service_tool_context(&tool_call_id, options.background);
                let outcome = host
                    .call_tool_with_context(&name, &tool_call_id, &args, &context)
                    .await
                    .map_err(|error| {
                        PluginServiceError::failed(format!("tool `{name}` failed: {error}"))
                    })?;
                batch_result_from_extension(outcome)
            } else {
                // Core/session tool: run the merged registry tool with the
                // loop's cwd/abort/update wiring.
                session
                    .run_native_service_tool(&name, &args, options.background, &tool_call_id)
                    .await
                    .map_err(PluginServiceError::failed)?
            };
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            Ok(NestedToolOutcome {
                tool_call_id,
                tool_name: name,
                is_error: result.is_error,
                result,
                duration_ms: Some(duration_ms),
                usage: None,
            })
        })
    }

    fn exec(
        &self,
        command: &str,
        args: Vec<String>,
        options: ExecOptions,
    ) -> ServiceFuture<'static, ServiceResult<ExecResult>> {
        let command = command.to_owned();
        let cwd = options.cwd.unwrap_or_else(|| self.state.cwd.to_path_buf());
        let timeout_ms = options.timeout_ms;
        let signal = options.signal;
        Box::pin(
            async move { Ok(run_native_exec(&command, &args, &cwd, timeout_ms, signal).await) },
        )
    }

    fn update(&self, call: &ToolCallRef, partial: JsonValue) {
        // The trait is synchronous; the unified plugin dispatch is async.
        // Schedule it on the caller's runtime without blocking. A dropped
        // session/runtime simply skips the fire-and-forget update.
        let Some(session) = self.state.session() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let call = call.clone();
        drop(runtime.spawn(async move {
            session.dispatch_native_tool_update(call, partial).await;
        }));
    }
}

/// Map the plugin host's ordered native-call outcome onto the agent-batch
/// result the nested outcome carries.
///
/// Text items become the single text body; the host's structured
/// [`ToolCallOutcome::images`] become the batch result's `{type:"image"}`
/// blocks; `details` / `structuredContent` are preserved only when the tool
/// supplied them (the host stores absent values as JSON `null`).
fn batch_result_from_extension(outcome: crate::plugins::ToolCallOutcome) -> BatchToolResult {
    let crate::plugins::ToolCallOutcome { result, images } = outcome;
    let crate::extensions::ToolResult {
        content,
        details,
        structured_content,
        terminate,
    } = result;
    BatchToolResult {
        content: content
            .iter()
            .filter(|item| item.kind == "text")
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        images,
        details: (!details.is_null()).then_some(details),
        structured_content: (!structured_content.is_null()).then_some(structured_content),
        is_error: false,
        terminate,
    }
}

/// Maximum bytes captured per stream for a native `pi.exec` call (the same
/// 1 MiB bound the JS face applies), with a truncation notice.
const MAX_EXEC_OUTPUT_BYTES: usize = 1024 * 1024;

/// Run one direct subprocess for `ToolService::exec`.
///
/// The command is spawned DIRECTLY with its argv (never through a shell — the
/// options carry no shell request), matching the JS `pi.exec` contract. The
/// child's stdin is null, both pipes are drained concurrently with the wait
/// (so output can never block the child), and cwd/timeout/abort are honored:
/// an abort or timeout kills the child and reports `killed: true`. This is
/// the same spawn shape the bash tool's per-call path uses, minus the shell.
async fn run_native_exec(
    command: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: Option<f64>,
    signal: Option<CancellationHandle>,
) -> ExecResult {
    use std::process::Stdio;

    let mut spawn = tokio::process::Command::new(command);
    spawn
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The bash tool's spawn seam: on Unix the child leads its own process
    // group so the tree kill below reaches its descendants; on Windows the
    // tree-kill handle arms the Job/sweep machinery at termination time.
    crate::coding_agent::tools::shell_pool::apply_tree_kill_spawn_attributes(&mut spawn);
    let mut child = match spawn.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ExecResult {
                stdout: String::new(),
                stderr: format!("failed to spawn `{command}`: {error}"),
                code: 1,
                killed: false,
            }
        }
    };
    let pid = child.id();
    let tree_kill = pid.map(|pid| {
        (
            crate::coding_agent::tools::shell_pool::ProcessTreeKill::attach(&child),
            pid,
        )
    });
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Upstream ignores falsy / non-positive timeouts (`exec.ts:75`).
    let deadline = timeout_ms.filter(|ms| *ms > 0.0).map(|ms| {
        tokio::time::Instant::now() + Duration::from_millis(ms.min(u32::MAX as f64) as u64)
    });
    let stdout_read = async move {
        match stdout {
            Some(pipe) => read_exec_pipe(pipe).await,
            None => String::new(),
        }
    };
    let stderr_read = async move {
        match stderr {
            Some(pipe) => read_exec_pipe(pipe).await,
            None => String::new(),
        }
    };
    let wait = async move {
        let mut killed = false;
        let mut kill_attempts = 0u32;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    let aborted = signal.as_ref().is_some_and(CancellationHandle::is_aborted);
                    let timed_out =
                        deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
                    if aborted || timed_out {
                        killed = true;
                        // Reuse the bash tool's tree kill: a direct child
                        // may have spawned descendants that a root-only
                        // `kill` would leave running.
                        if let Some((tree_kill, pid)) = tree_kill.as_ref() {
                            tree_kill.terminate(*pid);
                        }
                        let _ = child.kill().await;
                        kill_attempts += 1;
                        // A child that somehow survives the kill must not keep
                        // this wait spinning forever (5 ms × 200 ≈ 1 s).
                        if kill_attempts > 200 {
                            break child.try_wait().ok().flatten();
                        }
                    }
                    // Same 5 ms poll granularity as the extension `pi.exec`
                    // worker; the wait never blocks an async worker thread.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(_) => break None,
            }
        };
        (status, killed)
    };
    let (stdout, stderr, (status, killed)) = tokio::join!(stdout_read, stderr_read, wait);
    ExecResult {
        stdout,
        stderr,
        // `code ?? 0` for an exited child (signal death reports 0, upstream
        // `exec.ts:97`) and `1` when the wait itself failed.
        code: status.and_then(|status| status.code()).unwrap_or(0),
        killed,
    }
}

/// Drain one exec pipe up to [`MAX_EXEC_OUTPUT_BYTES`], flagging truncation
/// (never silently dropping output, and never stopping early: the reader
/// drains to EOF so the writer cannot block on a full pipe).
async fn read_exec_pipe<R: tokio::io::AsyncRead + Unpin>(mut pipe: R) -> String {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let room = MAX_EXEC_OUTPUT_BYTES.saturating_sub(captured.len());
                if count > room {
                    captured.extend_from_slice(&buffer[..room]);
                    truncated = true;
                } else {
                    captured.extend_from_slice(&buffer[..count]);
                }
            }
        }
    }
    let mut text = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    text
}

impl AgentSession {
    /// The [`ToolContext`] a native [`ToolService`] call runs under.
    ///
    /// [`ToolService::execute_tool`] carries only the caller's call id and
    /// the per-call options (there is no context parameter on the service
    /// trait), so the context is rebuilt from the session's live wiring —
    /// every field a scheduled call receives:
    ///
    /// - `cwd`: the session working directory;
    /// - `signal`: the session's live abort signal (fired by
    ///   [`AgentSession::abort`], observed by the tool);
    /// - `session_env`: the same model/thinking/tool/background environment
    ///   the agent loop builds for a run, so tools that spawn agent runs
    ///   (`subagent`) see the real session;
    /// - `nested`: a nested-call executor rooted at `call_id`, so a native
    ///   plugin tool may itself execute further tools (`<call_id>/<n>`);
    /// - `on_update`: the live partial-result channel (TUI streaming and the
    ///   plugin `tool_execution_update` fan-out);
    /// - `background`: the caller's `options.background`;
    /// - `held_mutation_keys`: empty — a service call is not a batch member
    ///   carrying an ancestor's per-file queue locks.
    fn native_service_tool_context(&self, call_id: &str, background: bool) -> ToolContext {
        let live_tools = match &self.plugins {
            Some(plugins) => self.merged_tool_registry(plugins),
            None => {
                let mut registry = (*self.tools).clone();
                registry.remove_exposure(ToolExposure::Hidden);
                Arc::new(registry)
            }
        };
        let signal = self
            .signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let on_update = self.tool_update_channel();
        let session_env = Arc::new(ToolSessionEnv {
            provider_registry: Arc::clone(&self.registry),
            model: self.current_model(),
            thinking: self.current_thinking(),
            session_id: self.session_id(),
            session_file: self.session_path(),
            base_tools: Arc::clone(&self.tools),
            // The session facade is headless: no UI host is attached to
            // `ToolSessionEnv` here (the same posture the agent loop takes
            // for this session).
            has_ui: false,
            project_trusted: true,
            // Same agent-directory identity the agent loop uses: subagent
            // discovery is rooted at `<agent-dir>/agents` for this session.
            user_agents_dir: Some(self.agent_dir().join("agents")),
            agent_dir: Some(self.agent_dir().to_path_buf()),
            credential: self.explicit_tool_credential(),
            on_update: on_update.clone(),
            live_tools: Some(Arc::clone(&live_tools)),
            active_override: Some(Arc::clone(&self.active_tools_override)),
            background: Some(Arc::new(
                crate::agent_session::background::SessionBackgroundControl::new(Arc::clone(
                    &self.background,
                )),
            )
                as Arc<dyn crate::agent::tool_execution::BackgroundControl>),
        });
        // A service call has no agent-loop event sink, so nested calls it
        // makes run without `tool_execution_start`/`end` observers; the
        // executor still routes through the session's scheduler (gates,
        // lanes, detach sink) and carries the session environment.
        let sink: crate::agent::nested_tool_calls::NestedEventSink =
            Arc::new(|_event| Box::pin(async {}));
        let shared = NestedShared::new(
            Arc::clone(&live_tools),
            self.scheduler.clone(),
            None,
            self.extensions.clone(),
            self.cwd.clone(),
            Some(signal.clone()),
            true,
            sink,
            Some(Arc::clone(&session_env)),
            None,
        );
        // Install the unified host so nested `tool_call`/`tool_result` gates
        // dispatch in the documented plugin order, exactly like a
        // model-issued call.
        if let Some(plugins) = &self.plugins {
            shared.configure_plugins(Some(Arc::clone(plugins)), None);
        }
        let nested = NestedToolExecutor::root(shared).for_call(call_id);
        ToolContext {
            cwd: self.cwd.clone(),
            signal: Some(signal),
            nested: Some(Arc::new(nested)),
            session_env: Some(session_env),
            held_mutation_keys: None,
            on_update,
            background,
        }
    }

    /// Run one CORE/session tool from the merged registry for the native
    /// [`ToolService`] (plugin-registered tools go through
    /// [`PluginHost::call_tool_with_context`] instead).
    ///
    /// The context is [`Self::native_service_tool_context`] — the same
    /// abort/session/update/background wiring a scheduled call gets. There
    /// is no batch scheduler here, so `background` only marks the context
    /// (shell tools use it to skip the settings timeout) rather than
    /// detaching the call.
    pub(crate) async fn run_native_service_tool(
        &self,
        name: &str,
        args: &JsonValue,
        background: bool,
        call_id: &str,
    ) -> Result<BatchToolResult, String> {
        let registry = match &self.plugins {
            Some(plugins) => self.merged_tool_registry(plugins),
            None => {
                let mut registry = (*self.tools).clone();
                registry.remove_exposure(ToolExposure::Hidden);
                Arc::new(registry)
            }
        };
        let Some(tool) = registry.get(name) else {
            return Err(format!("tool `{name}` is not registered by the session"));
        };
        let context = self.native_service_tool_context(call_id, background);
        Ok(tool.run(args, &context).await)
    }
}

// ---------------------------------------------------------------------------
// UiService
// ---------------------------------------------------------------------------

/// Live UI surface over the ONE presentation queue both faces share.
///
/// Every write is a [`Notification`] queued on the JS runtime's bounded
/// notification queue (the same values `ctx.ui.*` host functions produce), so
/// the TUI drain, the headless native mirror, and `take_notifications`
/// observe native and JS UI calls identically — no second queue, no second
/// payload shape.
///
/// Interactive dialogs are live too, over that same queue: the request is an
/// ordinary [`Notification::DialogRequest`] with an id allocated from the
/// host's [`NativeDialogCell`] (the disjoint top half of the shared id
/// space), and the reply arrives when the TUI's dialog responder resolves the
/// cell entry. See "Native dialogs" in the module docs.
struct LiveUiService {
    state: Arc<LiveSessionState>,
}

impl LiveUiService {
    fn js(&self) -> Option<&ExtensionHandle> {
        self.state.js.as_ref()
    }

    fn push(&self, notification: Notification) {
        if let Some(js) = self.js() {
            js.push_notification(notification);
        }
    }

    /// Ask the TUI to re-render every factory slot on its next tick (the
    /// native twin of a `tui.requestRender()` call). Used when a native
    /// removal reveals a still-registered JS factory: the slot must stay in
    /// the `FactoryWidget` cache, only its lines are stale.
    fn push_factory_repaint(&self) {
        self.push(Notification::Status {
            key: FACTORY_RENDER_REQUEST_KEY.to_owned(),
            text: None,
        });
    }

    /// Whether the JS face still has a registration for `(kind, key)` in the
    /// exported renderer registry.
    ///
    /// The widget bucket is keyed by `customType`; the footer/header slots are
    /// single-slot buckets exported as a one-element list (see the prelude's
    /// `__pir_exportRegistryJson`). A stale snapshot (a registration landing
    /// concurrently) can only make this answer `false` early; the TUI's own
    /// JS path then re-registers the slot on its next notification.
    fn js_factory_registered(&self, kind: RendererKind, key: &str) -> bool {
        let Some(js) = self.js() else {
            return false;
        };
        let bucket = match kind {
            RendererKind::Widget => "widgetRenderers",
            RendererKind::Footer => "footerRenderers",
            RendererKind::Header => "headerRenderers",
            _ => return false,
        };
        let defs = js.renderer_defs();
        let Some(entries) = defs.get(bucket).and_then(JsonValue::as_array) else {
            return false;
        };
        match kind {
            RendererKind::Widget => entries
                .iter()
                .any(|entry| entry.get("customType").and_then(JsonValue::as_str) == Some(key)),
            _ => !entries.is_empty(),
        }
    }

    fn theme_registry(&self) -> Option<JsonValue> {
        self.js().map(ExtensionHandle::theme_registry)
    }

    /// The native plugin host holding the dialog reply cell and the native
    /// event fan-out (`ui_prompt_*`), when the session has one.
    fn host(&self) -> Option<Arc<PluginHost>> {
        self.state.host()
    }

    /// Start one native dialog on the shared presentation queue.
    ///
    /// The returned future brackets the wait with upstream's
    /// `ui_prompt_start` / `ui_prompt_end` pair through the native fan-out
    /// ([`native_ui_prompt`]). The request itself is registered and queued
    /// synchronously at call time, exactly as before: hosts that inspect the
    /// presentation queue immediately after the call keep observing one
    /// request per call.
    ///
    /// Every pre-flight check runs synchronously (the service method itself
    /// is synchronous even though it returns a future), matching where the JS
    /// `uiDialog` host fn checks `has_ui`: a request made inside a renderer
    /// callback is suppressed at call time, not later when the future is
    /// first polled.
    fn start_dialog(
        &self,
        kind: &str,
        capability: &'static str,
        title: &str,
        options: JsonValue,
        timeout: Option<Duration>,
    ) -> ServiceFuture<'static, ServiceResult<Option<JsonValue>>> {
        let Some(queue) = self.js().cloned() else {
            return Box::pin(async move { Err(PluginServiceError::capability("ui", capability)) });
        };
        // A session configured without a UI sink has no drain and no way to
        // answer: keep the documented fail-closed contract.
        if !self.state.has_ui {
            return Box::pin(async move { Err(PluginServiceError::capability("ui", capability)) });
        }
        // The renderer re-entrancy guard's observable effect: while a
        // renderer/factory callback owns the UI thread the runtime's
        // `has_ui` is false and the JS dialog host fn resolves `null`
        // immediately. Resolve `None` here for the same window instead of
        // parking a request only the blocked thread could drain.
        if !queue.config().has_ui {
            return Box::pin(async move { Ok(None) });
        }
        let Some(host) = self.state.host() else {
            return Box::pin(async move { Err(PluginServiceError::capability("ui", capability)) });
        };
        let cell = host.native_dialogs();
        // Reserve the reply slot BEFORE publishing the request: a reply can
        // never race ahead of the registration.
        let (id, rx) = cell.register();
        // ONE queue, ONE wire shape: exactly the id/kind/title/options fields
        // the JS host fn pushes. No mirror notification, so a drain observes
        // one request per call.
        queue.push_notification(Notification::DialogRequest {
            id,
            kind: kind.to_owned(),
            title: Some(title.to_owned()),
            options,
        });
        let wait = NativeDialogWait {
            cell,
            queue,
            id,
            rx: Some(rx),
            timeout,
            capability,
        }
        .wait();
        Box::pin(native_ui_prompt(
            self.host(),
            kind.to_owned(),
            title.to_owned(),
            Box::pin(wait),
        ))
    }
}

/// The upstream `ui_prompt_start` / `ui_prompt_end` payload
/// (`{type, reason, kind, title?}`).
///
/// The optional title follows upstream's truthiness rule (an empty title is
/// omitted, matching the prelude's `...(title ? { title } : {})`).
fn ui_prompt_payload(event_type: &str, kind: &str, title: &str) -> JsonValue {
    let mut payload = serde_json::Map::with_capacity(4);
    payload.insert("type".to_owned(), JsonValue::String(event_type.to_owned()));
    payload.insert(
        "reason".to_owned(),
        JsonValue::String("ui_prompt".to_owned()),
    );
    payload.insert("kind".to_owned(), JsonValue::String(kind.to_owned()));
    if !title.is_empty() {
        payload.insert("title".to_owned(), JsonValue::String(title.to_owned()));
    }
    JsonValue::Object(payload)
}

/// One in-flight native UI prompt's event scope.
///
/// Upstream's runner wraps every extension UI dialog in `withUIPrompt`, which
/// emits `ui_prompt_start` for the OUTERMOST prompt and one matching
/// `ui_prompt_end` when the last nested prompt finishes (runner.ts:589-621).
/// The JS prelude already does that for JS dialogs; this guard is the native
/// service's half and shares the outermost depth on [`PluginHost`], so
/// concurrent/nested native dialogs on the same host produce exactly one pair
/// carrying the outer prompt's kind/title. It writes through
/// [`PluginHost::emit_native`] only: a native dialog's prompt events are the
/// native face's lifecycle, and the JS adapter must never observe a second
/// copy of an event its prelude did not produce.
struct NativeUiPrompt {
    host: Arc<PluginHost>,
    /// Set once the depth entry is registered: only then may a drop balance
    /// it with the matching end.
    armed: bool,
    /// Set once the end was emitted (or scheduled by a drop): exactly-once.
    ended: bool,
}

impl NativeUiPrompt {
    fn new(host: Arc<PluginHost>) -> Self {
        Self {
            host,
            armed: false,
            ended: false,
        }
    }

    /// Enter the shared prompt scope and dispatch `ui_prompt_start` only when
    /// this dialog is the outermost one.
    async fn start(&mut self, kind: &str, title: &str) {
        let outermost = self.host.begin_ui_prompt(kind, title);
        self.armed = true;
        if outermost {
            let mut payload = ui_prompt_payload("ui_prompt_start", kind, title);
            self.host.emit_native("ui_prompt_start", &mut payload).await;
        }
    }

    /// Leave the shared prompt scope; the dialog that closes the OUTERMOST
    /// prompt dispatches the single `ui_prompt_end` with the outer
    /// kind/title (upstream `activeUIPrompt`).
    async fn finish(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        if !self.armed {
            return;
        }
        if let Some((kind, title)) = self.host.end_ui_prompt() {
            let mut payload = ui_prompt_payload("ui_prompt_end", &kind, &title);
            self.host.emit_native("ui_prompt_end", &mut payload).await;
        }
    }
}

impl Drop for NativeUiPrompt {
    fn drop(&mut self) {
        if self.ended || !self.armed {
            return;
        }
        self.ended = true;
        // The dialog future was dropped (caller cancellation): the depth must
        // still be balanced and the end event must reach the native face when
        // this was the outermost prompt. `Drop` cannot await, so schedule the
        // dispatch on the caller's runtime; without one there is no executor
        // to deliver it (the dialog itself could not have run either), so the
        // drop is logged rather than fabricated.
        let host = Arc::clone(&self.host);
        let end_payload = host
            .end_ui_prompt()
            .map(|(kind, title)| ui_prompt_payload("ui_prompt_end", &kind, &title));
        match (end_payload, tokio::runtime::Handle::try_current()) {
            (Some(mut payload), Ok(handle)) => {
                handle.spawn(async move {
                    host.emit_native("ui_prompt_end", &mut payload).await;
                });
            }
            (Some(_), Err(_)) => crate::ext_diag!(
                "[ext] ui_prompt_end for a dropped native dialog had no runtime to dispatch on"
            ),
            // A nested prompt closed: the outermost dialog still owns the
            // eventual end event.
            (None, _) => {}
        }
    }
}

/// Run one native dialog future inside the upstream `ui_prompt_start` /
/// `ui_prompt_end` bracket. Without a plugin host the dialog still runs; there
/// is simply no native face to notify.
async fn native_ui_prompt(
    host: Option<Arc<PluginHost>>,
    kind: String,
    title: String,
    dialog: ServiceFuture<'static, ServiceResult<Option<JsonValue>>>,
) -> ServiceResult<Option<JsonValue>> {
    let Some(host) = host else {
        return dialog.await;
    };
    let mut prompt = NativeUiPrompt::new(host);
    prompt.start(&kind, &title).await;
    let result = dialog.await;
    prompt.finish().await;
    result
}

/// One in-flight native dialog: the host reply cell entry plus the queue
/// handle needed to withdraw the request if the future is dropped first.
struct NativeDialogWait {
    cell: Arc<NativeDialogCell>,
    /// The runtime whose presentation queue holds the request (used only for
    /// cancellation cleanup).
    queue: ExtensionHandle,
    id: u64,
    rx: Option<tokio::sync::oneshot::Receiver<JsonValue>>,
    /// Request-level deadline (`DialogOptions::timeout`).
    timeout: Option<Duration>,
    capability: &'static str,
}

impl NativeDialogWait {
    async fn wait(mut self) -> ServiceResult<Option<JsonValue>> {
        let Some(mut rx) = self.rx.take() else {
            // One-shot by construction; a second poll is a host bug, not a
            // panic path.
            return Err(PluginServiceError::Failed(
                "native dialog wait was already consumed".to_owned(),
            ));
        };
        let replied = match self.timeout {
            Some(timeout) => match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    // A `Sleep` needs a timer context when constructed; build
                    // it under the caller's runtime, then race the wakers.
                    let sleep = {
                        let _entered = handle.enter();
                        tokio::time::sleep(timeout)
                    };
                    tokio::select! {
                        reply = &mut rx => Some(reply),
                        _ = sleep => None,
                    }
                }
                // No reactor: the caller's executor owns the clock. The wait
                // stays interruptible by the reply channel.
                Err(_) => Some(rx.await),
            },
            None => Some(rx.await),
        };
        match replied {
            Some(Ok(value)) => Ok(Some(value)),
            // The sender lives in the host cell; a closed channel means the
            // cell/host is gone rather than a user answer.
            Some(Err(_)) => Err(PluginServiceError::failed(format!(
                "ui `{}` dialog reply channel closed",
                self.capability
            ))),
            None => Err(PluginServiceError::failed(format!(
                "ui `{}` dialog timed out",
                self.capability
            ))),
        }
    }
}

impl Drop for NativeDialogWait {
    fn drop(&mut self) {
        self.cell.cancel(self.id);
        // Best effort: withdraw the request while it still sits in the shared
        // presentation queue. If the TUI already drained it into an overlay,
        // the overlay stays visible and its eventual reply finds no waiter
        // (`respond` returns false), so nothing leaks either way.
        let _ = self.queue.take_notifications(|notification| {
            matches!(
                notification,
                Notification::DialogRequest { id, .. } if *id == self.id
            )
        });
    }
}

/// One in-flight native `ctx.ui.custom` overlay: the ordinary dialog wait PLUS
/// ownership of the component registration the TUI takes when it admits the
/// request.
///
/// Dropping this future (caller cancellation, timeout, a session switch) drops
/// a registration the TUI never took, so the component is disposed exactly
/// once instead of leaking in the host-scoped registry. When the TUI took it,
/// the removal is a no-op.
struct CustomOverlayWait {
    inner: ServiceFuture<'static, ServiceResult<Option<JsonValue>>>,
    registries: Arc<NativeRegistries>,
    id: u64,
}

impl std::future::Future for CustomOverlayWait {
    type Output = ServiceResult<Option<JsonValue>>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // Every field is `Unpin` (`Pin<Box<dyn Future>>` included), so the
        // projection needs no `unsafe`.
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll(cx) {
            std::task::Poll::Ready(result) => {
                this.registries.remove_overlay_component(this.id);
                std::task::Poll::Ready(match result {
                    // `null` is the dismissal answer upstream resolves as
                    // `undefined`; everything else is the factory's
                    // `done(result)` value.
                    Ok(Some(JsonValue::Null)) | Ok(None) => Ok(None),
                    Ok(Some(value)) => Ok(Some(value)),
                    Err(error) => Err(error),
                })
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl Drop for CustomOverlayWait {
    fn drop(&mut self) {
        self.registries.remove_overlay_component(self.id);
    }
}

/// Add the request-level deadline to the shared dialog wire payload: the same
/// `timeout` key the JS prelude's `opts` spread carries and the TUI countdown
/// reads.
fn insert_dialog_timeout(
    payload: &mut serde_json::Map<String, JsonValue>,
    timeout: Option<Duration>,
) {
    if let Some(timeout) = timeout {
        payload.insert(
            "timeout".to_owned(),
            serde_json::json!(timeout.as_millis() as u64),
        );
    }
}

/// The TUI dock slot for a native widget placement (the prelude-normalized
/// wire values the `FactoryWidget` cache filters on).
///
/// `Overlay` has no dedicated dock surface: like the static-widget path
/// (`LiveUiService::set_widget_static`) it renders in the above-transcript
/// slot, which is the closest existing surface.
fn widget_slot(placement: WidgetPlacement) -> &'static str {
    match placement {
        WidgetPlacement::BelowEditor => "below_transcript",
        WidgetPlacement::AboveEditor | WidgetPlacement::Overlay => "above_transcript",
    }
}

/// `select` / `input` / `editor` result mapping: a string answer is the
/// value; `null` (Esc/cancel or a TUI timeout reply) and any non-string
/// payload are `None`.
fn string_dialog_reply(reply: Option<JsonValue>) -> Option<String> {
    match reply {
        Some(JsonValue::String(text)) => Some(text),
        _ => None,
    }
}

/// One native `ctx.ui.custom` request, shared by the legacy
/// [`UiService::custom`] method and the typed
/// [`crate::agent_session::NativeSessionHost::custom_overlay`] path.
///
/// `overlay` opts the request into the TUI's floating-overlay stack: the
/// payload carries the exact `overlay: true` + `overlayOptions` keys the JS
/// prelude emits and `tui::overlay_position::OverlayOptions::from_request`
/// already parses, so no TUI signature changes are involved. `handle`, when
/// present, is bound to the allocated dialog id before the request is queued
/// (hide/unhide and the `getBounds` cell).
///
/// A factory built as a [`crate::agent_session::NativeOverlayFactory`]
/// registers its option bag during `create`; it is consumed immediately after
/// the factory returns, which lets the EXISTING trait method float without
/// extending its signature.
#[allow(clippy::too_many_arguments)]
fn start_custom_overlay(
    js: Option<ExtensionHandle>,
    host: Option<Arc<PluginHost>>,
    has_ui: bool,
    registries: Arc<NativeRegistries>,
    factory: Arc<dyn OverlayComponentFactory>,
    timeout: Option<Duration>,
    overlay: Option<NativeOverlayOptions>,
    handle: Option<NativeOverlayHandle>,
) -> ServiceFuture<'static, ServiceResult<Option<JsonValue>>> {
    let Some(queue) = js else {
        return Box::pin(async move { Err(PluginServiceError::capability("ui", "custom")) });
    };
    if !has_ui {
        return Box::pin(async move { Err(PluginServiceError::capability("ui", "custom")) });
    }
    // The renderer re-entrancy window (see `start_dialog`): a dialog
    // requested from inside a renderer/factory callback would park a
    // request only the blocked UI thread could answer.
    if !queue.config().has_ui {
        return Box::pin(async move { Ok(None) });
    }
    let Some(host) = host else {
        return Box::pin(async move { Err(PluginServiceError::capability("ui", "custom")) });
    };
    let cell = host.native_dialogs();
    let (id, rx) = cell.register();
    // Build the component BEFORE publishing the request: a factory that
    // declines must not leave a request in the queue that the TUI would
    // answer with a dangling component.
    //
    // `done(result)` reports over the SAME reserved channel the JS face
    // uses; the TUI's drain settles the overlay (dispose + promote + reply
    // routing) exactly once, so both faces share one completion path.
    let done: OverlayDone = {
        let queue = queue.clone();
        Arc::new(move |value: JsonValue| {
            queue.push_notification(Notification::Status {
                key: CUSTOM_OVERLAY_EVENT_KEY.to_owned(),
                text: Some(
                    serde_json::json!({
                        "component": format!("native:{id}"),
                        "op": "done",
                        "result": value,
                    })
                    .to_string(),
                ),
            });
        })
    };
    let created = catch_native_callback("custom overlay factory", || factory.create(done));
    // Consume the option bag a `NativeOverlayFactory` registered during
    // `create` (one register per create, one consume per custom call). The
    // pointer is the trait object's data address, identical to the concrete
    // wrapper's allocation address.
    let spec = crate::extensions::overlay::take_overlay_factory_spec(
        Arc::as_ptr(&factory) as *const () as usize
    );
    let overlay = overlay.or_else(|| spec.as_ref().map(|spec| spec.options.clone()));
    let handle = handle.or_else(|| spec.map(|spec| spec.handle));
    let component = match created {
        Some(Some(component)) => component,
        // Declined or panicked: release the waiter slot and fail closed.
        Some(None) | None => {
            cell.cancel(id);
            return Box::pin(async move {
                Err(PluginServiceError::failed(
                    "ui `custom` factory did not produce a component",
                ))
            });
        }
    };
    registries.set_overlay_component(id, Arc::clone(&component));
    let mut payload = serde_json::Map::new();
    // The one wire field both faces share: which component the TUI shows.
    payload.insert(
        "component".to_owned(),
        JsonValue::String(format!("native:{id}")),
    );
    if let Some(options) = &overlay {
        // Upstream `showExtensionCustom`: float on the overlay stack,
        // positioned by the typed bag the TUI already parses.
        payload.insert("overlay".to_owned(), JsonValue::Bool(true));
        payload.insert("overlayOptions".to_owned(), options.to_wire());
    }
    if let Some(handle) = &handle {
        // Bind before the request is queued: a `hidden` set by the caller
        // rides the opening payload (the JS prelude's no-flash rule).
        handle.bind(id, &queue);
        if handle.is_hidden() {
            payload.insert("hidden".to_owned(), JsonValue::Bool(true));
        }
    }
    insert_dialog_timeout(&mut payload, timeout);
    queue.push_notification(Notification::DialogRequest {
        id,
        kind: crate::plugins::plugin::services::CUSTOM_OVERLAY_DIALOG_KIND.to_owned(),
        title: None,
        options: JsonValue::Object(payload),
    });
    let wait = NativeDialogWait {
        cell: Arc::clone(&cell),
        queue,
        id,
        rx: Some(rx),
        timeout,
        capability: "custom",
    }
    .wait();
    // The overlay registry entry belongs to this request: if the wait ends
    // without the TUI taking it (a queued request the waiter abandoned, a
    // timeout), drop the registration so the component is disposed exactly
    // once instead of leaking in the host-scoped registry.
    Box::pin(native_ui_prompt(
        Some(host),
        crate::plugins::plugin::services::CUSTOM_OVERLAY_DIALOG_KIND.to_owned(),
        String::new(),
        Box::pin(CustomOverlayWait {
            inner: Box::pin(wait),
            registries,
            id,
        }),
    ))
}

/// [`crate::agent_session::NativeSessionHost::custom_overlay`]'s execution
/// path: resolve the session's presentation queue + host-scoped registries and
/// hand the typed option bag to the one custom-overlay funnel.
pub(crate) fn native_custom_overlay_request(
    session: &AgentSession,
    factory: Arc<dyn OverlayComponentFactory>,
    options: NativeOverlayOptions,
    handle: NativeOverlayHandle,
    timeout: Option<Duration>,
) -> ServiceFuture<'static, ServiceResult<Option<JsonValue>>> {
    let js = session.extensions.clone();
    let has_ui = js.as_ref().is_some_and(|js| js.config().has_ui);
    let Some(host) = session.plugins.clone() else {
        return Box::pin(async move { Err(PluginServiceError::capability("ui", "custom")) });
    };
    let registries = shared_native_registries(&host);
    start_custom_overlay(
        js,
        Some(host),
        has_ui,
        registries,
        factory,
        timeout,
        Some(options),
        Some(handle),
    )
}

impl UiService for LiveUiService {
    fn notify(&self, level: NotifyLevel, message: &str) {
        self.push(Notification::Notify {
            message: message.to_owned(),
            level: level.as_str().to_owned(),
        });
    }

    fn select(
        &self,
        title: &str,
        options: &[SelectOption],
        dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        // The prelude's wire shape: string choices become `{value, label}`,
        // then `opts` (the request-level timeout) is spread in.
        let choices = options
            .iter()
            .map(|option| {
                serde_json::json!({
                    "value": option.value,
                    "label": option.label,
                })
            })
            .collect::<Vec<_>>();
        let mut payload = serde_json::Map::new();
        payload.insert("title".to_owned(), JsonValue::String(title.to_owned()));
        payload.insert("options".to_owned(), JsonValue::Array(choices));
        insert_dialog_timeout(&mut payload, dialog.timeout);
        let request = self.start_dialog(
            "select",
            "select",
            title,
            JsonValue::Object(payload),
            dialog.timeout,
        );
        Box::pin(async move { Ok(string_dialog_reply(request.await?)) })
    }

    fn confirm(
        &self,
        title: &str,
        message: &str,
        dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<bool>>> {
        let mut payload = serde_json::Map::new();
        payload.insert("title".to_owned(), JsonValue::String(title.to_owned()));
        payload.insert("message".to_owned(), JsonValue::String(message.to_owned()));
        insert_dialog_timeout(&mut payload, dialog.timeout);
        let request = self.start_dialog(
            "confirm",
            "confirm",
            title,
            JsonValue::Object(payload),
            dialog.timeout,
        );
        Box::pin(async move {
            Ok(match request.await? {
                Some(JsonValue::Bool(value)) => Some(value),
                // `null` is Esc/cancel; a non-boolean answer is not a confirm
                // result (the JS face applies the same `typeof` check).
                _ => None,
            })
        })
    }

    fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        let mut payload = serde_json::Map::new();
        payload.insert("title".to_owned(), JsonValue::String(title.to_owned()));
        if let Some(placeholder) = placeholder {
            payload.insert(
                "placeholder".to_owned(),
                JsonValue::String(placeholder.to_owned()),
            );
        }
        insert_dialog_timeout(&mut payload, dialog.timeout);
        let request = self.start_dialog(
            "input",
            "input",
            title,
            JsonValue::Object(payload),
            dialog.timeout,
        );
        Box::pin(async move { Ok(string_dialog_reply(request.await?)) })
    }

    fn editor(
        &self,
        title: &str,
        prefill: Option<&str>,
        dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        let mut payload = serde_json::Map::new();
        payload.insert("title".to_owned(), JsonValue::String(title.to_owned()));
        if let Some(prefill) = prefill {
            payload.insert("prefill".to_owned(), JsonValue::String(prefill.to_owned()));
        }
        // The native trait carries a deadline for every dialog; the TUI
        // honors the same `timeout` key for editor overlays (the JS prelude's
        // editor wrapper does not forward opts, so only native requests carry
        // it there).
        insert_dialog_timeout(&mut payload, dialog.timeout);
        let request = self.start_dialog(
            "editor",
            "editor",
            title,
            JsonValue::Object(payload),
            dialog.timeout,
        );
        Box::pin(async move { Ok(string_dialog_reply(request.await?)) })
    }

    fn set_working_message(&self, message: Option<&str>) {
        // The JS host fn encodes "restore the default" as the empty string.
        self.push(Notification::WorkingMessage(
            message.unwrap_or_default().to_owned(),
        ));
    }

    fn set_working_visible(&self, visible: bool) {
        self.push(Notification::WorkingVisible(visible));
    }

    fn set_working_indicator(&self, options: Option<WorkingIndicator>) {
        // `None` restores the default spinner (`Null`); `Some` carries only
        // the fields the JS prelude would have serialized.
        let value = match options {
            None => JsonValue::Null,
            Some(indicator) => {
                let mut payload = serde_json::Map::new();
                if let Some(frames) = indicator.frames {
                    payload.insert("frames".to_owned(), serde_json::json!(frames));
                }
                if let Some(interval_ms) = indicator.interval_ms {
                    payload.insert("intervalMs".to_owned(), serde_json::json!(interval_ms));
                }
                JsonValue::Object(payload)
            }
        };
        self.push(Notification::WorkingIndicator(value));
    }

    fn set_hidden_thinking_label(&self, label: Option<&str>) {
        self.push(Notification::HiddenThinkingLabel(label.map(str::to_owned)));
    }

    /// `ctx.ui.custom(factory, options)` — the native face of upstream's
    /// custom component overlay (types.ts:213-229,
    /// `showExtensionCustom`, interactive-mode.ts:2966-3026).
    ///
    /// It rides the SAME request/reply path as
    /// [`Self::select`]/`confirm`/`input`/`editor` — one `DialogRequest`, one
    /// native dialog id, one bounded wait on [`NativeDialogCell`] — and the
    /// component itself is published in the host-scoped overlay registry,
    /// where the TUI's generic external-overlay slot takes it when it admits
    /// the request. Nothing here paints, routes keys or settles; the TUI's one
    /// overlay mechanism does all of it, exactly as it does for the typed
    /// prompts.
    ///
    /// Failure behaviour (all non-fatal, never a wedged await):
    /// - a factory that returns `None` or panics fails closed with
    ///   [`PluginServiceError::Failed`] and a transcript notice;
    /// - a dismissal/timeout resolves `Ok(None)` (JS `undefined`), which is
    ///   upstream's own dismissal contract.
    fn custom(
        &self,
        factory: Arc<dyn OverlayComponentFactory>,
        options: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<JsonValue>>> {
        // Floating options ride the factory when it is a native
        // `NativeOverlayFactory` (see `start_custom_overlay`); without one the
        // long-standing editor-slot default is unchanged.
        start_custom_overlay(
            self.js().cloned(),
            self.state.host(),
            self.state.has_ui,
            Arc::clone(&self.state.factories),
            factory,
            options.timeout,
            None,
            None,
        )
    }

    fn set_title(&self, title: &str) {
        self.push(Notification::SetTitle(title.to_owned()));
    }

    fn set_widget_static(&self, key: &str, lines: &[String], placement: WidgetPlacement) {
        // A static (re-)registration replaces a native factory under the same
        // key (upstream `setWidget` last-wins); retiring the native entry
        // first keeps the TUI's removal notification effective.
        self.state.factories.remove_widget(key);
        // The prelude's static-widget wire shape (TUI host):
        // `{text, placement: "above_transcript" | "below_transcript"}`.
        let placement = widget_slot(placement);
        self.push(Notification::Widget {
            id: key.to_owned(),
            content: serde_json::json!({
                "text": lines.join("\n"),
                "placement": placement,
            }),
        });
    }

    fn set_widget(&self, key: &str, factory: Arc<dyn WidgetFactory>, placement: WidgetPlacement) {
        // The factory lives in the host-scoped native registry (the renderer
        // service resolves it for the TUI); the SAME presentation
        // notification the JS prelude produces registers the key in the one
        // `FactoryWidget` cache (upsert + static retirement + dirty), so
        // there is no second cache and no second refresh loop.
        self.state.factories.set_widget(key, factory, placement);
        self.push(Notification::Widget {
            id: key.to_owned(),
            content: serde_json::json!({
                "factory": true,
                "placement": widget_slot(placement),
            }),
        });
    }

    fn clear_widget(&self, key: &str) {
        // Face-scoped removal: only a native registration is retired. A
        // clear for a slot the native face never owned must not touch the JS
        // registration.
        if !self.state.factories.remove_widget(key) {
            return;
        }
        if self.js_factory_registered(RendererKind::Widget, key) {
            // A JS factory still owns this key: keep the cache entry, just
            // re-render it through the JS face on the next tick.
            self.push_factory_repaint();
        } else {
            // No JS factory: the same removal payload `setWidget(key,
            // undefined)` produces (the TUI drops the cache entry).
            self.push(Notification::Widget {
                id: key.to_owned(),
                content: JsonValue::Null,
            });
        }
    }

    fn set_footer(&self, factory: Option<Arc<dyn BarFactory>>) {
        match factory {
            Some(factory) => {
                self.state.factories.set_footer(Some(factory));
                // The same registration signal the JS `setFooter(factory)`
                // host fn emits: register the TUI's single factory slot.
                self.push(Notification::Footer(true));
            }
            None => {
                if !self.state.factories.set_footer(None) {
                    // Nothing native-owned: never clear a JS registration.
                    return;
                }
                if self.js_factory_registered(RendererKind::Footer, "") {
                    // A JS footer remains registered: re-render the slot
                    // through the JS face instead of clearing it.
                    self.push_factory_repaint();
                } else {
                    self.push(Notification::Footer(false));
                }
            }
        }
    }

    fn set_header(&self, factory: Option<Arc<dyn BarFactory>>) {
        match factory {
            Some(factory) => {
                self.state.factories.set_header(Some(factory));
                self.push(Notification::Header(true));
            }
            None => {
                if !self.state.factories.set_header(None) {
                    return;
                }
                if self.js_factory_registered(RendererKind::Header, "") {
                    self.push_factory_repaint();
                } else {
                    self.push(Notification::Header(false));
                }
            }
        }
    }

    fn paste_to_editor(&self, text: &str) {
        self.push(Notification::EditorPaste(text.to_owned()));
    }

    fn set_editor_text(&self, text: &str) {
        self.push(Notification::EditorSetText(text.to_owned()));
    }

    fn editor_text(&self) -> Arc<str> {
        // The TUI pushes the live editor snapshot (`update_editor_state`);
        // headless hosts never push, so this reads the same empty cell the JS
        // `ctx.ui.getEditorText()` reads.
        match self.js() {
            Some(js) => Arc::from(js.editor_text().as_str()),
            None => Arc::from(""),
        }
    }

    fn theme(&self) -> Option<Arc<ThemeSnapshot>> {
        let registry = self.theme_registry()?;
        let name = registry.get("current").and_then(JsonValue::as_str)?;
        let data = registry.get("snapshots")?.get(name)?.clone();
        Some(Arc::new(ThemeSnapshot {
            name: Arc::from(name),
            data: Arc::new(data),
        }))
    }

    fn themes(&self) -> Arc<[ThemeInfo]> {
        let Some(registry) = self.theme_registry() else {
            return Arc::from(Vec::new());
        };
        let rows = registry
            .get("themes")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default();
        Arc::from(
            rows.iter()
                .filter_map(|row| {
                    let name = row.get("name").and_then(JsonValue::as_str)?;
                    let path = row
                        .get("path")
                        .and_then(JsonValue::as_str)
                        .map(|path| Arc::from(Path::new(path)));
                    Some(ThemeInfo {
                        name: Arc::from(name),
                        path,
                    })
                })
                .collect::<Vec<_>>(),
        )
    }

    fn set_theme(&self, name: &str) -> ThemeSetResult {
        // Mirror the JS `setThemeJson` host fn: validate against the pushed
        // theme registry, update its `current` slot, and queue the SAME
        // `SetTheme` notification the JS face produces so the TUI applies it.
        let Some(js) = self.js() else {
            return ThemeSetResult::Unavailable;
        };
        let registry = js.theme_registry();
        let known = registry
            .get("themes")
            .and_then(JsonValue::as_array)
            .is_some_and(|themes| {
                themes
                    .iter()
                    .any(|theme| theme.get("name").and_then(JsonValue::as_str) == Some(name))
            });
        if !known {
            return ThemeSetResult::NotFound;
        }
        js.set_current_theme(name);
        js.push_notification(Notification::SetTheme(name.to_owned()));
        ThemeSetResult::Applied
    }

    fn tools_expanded(&self) -> bool {
        // The JS face's `ctx.ui.getToolsExpanded` reads the SAME runtime cell
        // the TUI clones for Ctrl+O and tool-block rendering, so a native
        // read observes a live TUI toggle without a second copy.
        self.js().is_some_and(ExtensionHandle::tools_expanded)
    }

    fn set_tools_expanded(&self, expanded: bool) {
        // `ExtensionHandle::set_tools_expanded` writes the runtime cell the JS
        // host fn also writes; the TUI's drain sees the change on its next
        // tick (`tools_expanded_seen`) and restyles the transcript.
        if let Some(js) = self.js() {
            js.set_tools_expanded(expanded);
        }
    }

    fn on_terminal_input(
        &self,
        handler: Arc<dyn crate::plugins::services::TerminalInputHandler>,
    ) -> crate::plugins::services::Subscription {
        // The native half of upstream `ctx.ui.onTerminalInput`: the handler
        // joins the host's registration-order registry, which the TUI's input
        // producer consults on the same `terminal_input` dispatch as the JS
        // face. No host (or a host bound by an older build) keeps the documented
        // no-op subscription.
        match self.host() {
            Some(host) => host.on_terminal_input(handler),
            None => crate::plugins::services::Subscription::noop(),
        }
    }
}

// ---------------------------------------------------------------------------
// ProviderService
// ---------------------------------------------------------------------------

struct LiveProviderService {
    state: Arc<LiveSessionState>,
    /// The native auth hooks this binding registered, kept for teardown.
    ///
    /// The process-wide [`provider_auth_hook_registry`] is the lookup table
    /// auth entry points use; this cell only records what THIS service owns so
    /// dropping the binding removes exactly its registrations (never a newer
    /// session's last-wins replacement).
    registered_auth_hooks: Mutex<HashMap<String, Arc<dyn ProviderAuthHooks>>>,
}

impl LiveProviderService {
    /// Provider ids with configured auth, under the host's permissive-empty
    /// convention (no readable environment = "cannot check", `has_auth` then
    /// answers `true`, exactly like the JS `hasConfiguredAuth`).
    ///
    /// The credential-environment read (settings + `models.json` + per-provider
    /// `auth.json` probes) is never performed on a warm call: a cached answer
    /// younger than [`AUTH_CACHE_TTL`] is returned as-is; an expired one is
    /// returned immediately while one background task refreshes it. Only the
    /// very first call (no cached answer) may block on the read. A concurrent
    /// cold caller seeing the in-flight slot answers permissively (`[]` = open)
    /// instead of stacking another blocking read.
    fn authenticated_providers(&self) -> Arc<[Arc<str>]> {
        let (stale, cold) = {
            let mut cache = self
                .state
                .auth
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match cache
                .snapshot
                .as_ref()
                .map(|(read_at, providers)| (read_at.elapsed(), Arc::clone(providers)))
            {
                Some((elapsed, providers)) if elapsed < AUTH_CACHE_TTL => {
                    return providers;
                }
                Some((_, providers)) => {
                    if cache.refreshing {
                        return providers;
                    }
                    cache.refreshing = true;
                    (Some(providers), false)
                }
                None => {
                    if cache.refreshing {
                        return Arc::from(Vec::new());
                    }
                    cache.refreshing = true;
                    (None, true)
                }
            }
        };
        if cold {
            // Cold first call: one synchronous read is allowed. The refresh
            // flag is cleared on every exit path, including a panic.
            let providers = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                load_authenticated_providers(&self.state.registry, &self.state.cwd)
            }))
            .unwrap_or_else(|_| Arc::from(Vec::new()));
            let mut cache = self
                .state
                .auth
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.snapshot = Some((Instant::now(), Arc::clone(&providers)));
            cache.refreshing = false;
            providers
        } else {
            self.spawn_auth_refresh();
            stale.unwrap_or_else(|| Arc::from(Vec::new()))
        }
    }

    /// Refresh the cached credential snapshot off the caller's thread.
    ///
    /// Uses tokio's blocking pool when a runtime is current (the usual async
    /// tool/executor path) and a plain thread for synchronous embedders. At
    /// most one task runs per binding because the caller sets `refreshing`
    /// under the cache lock before spawning; the task clears it on completion
    /// (including a panic in the read, which is caught so the slot cannot stay
    /// wedged).
    fn spawn_auth_refresh(&self) {
        let registry = Arc::clone(&self.state.registry);
        let cwd = Arc::clone(&self.state.cwd);
        let auth = Arc::clone(&self.state.auth);
        let refresh = move || {
            let providers = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                load_authenticated_providers(&registry, &cwd)
            }))
            .unwrap_or_else(|_| Arc::from(Vec::new()));
            let mut cache = auth
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.snapshot = Some((Instant::now(), providers));
            cache.refreshing = false;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Detached: `has_auth` is synchronous and must not await the
                // refresh. A shutdown runtime simply drops the queued task;
                // the join handle is intentionally discarded.
                drop(handle.spawn_blocking(refresh));
            }
            Err(_) => {
                let _ = std::thread::spawn(refresh);
            }
        }
    }
}

/// One credential-environment read shared by the cold and background paths.
/// Counted by the host's `#[doc(hidden)]` `live_auth_disk_reads` probe so a
/// test can pin "one read for N hot-path calls".
fn load_authenticated_providers(registry: &ProviderRegistry, cwd: &Path) -> Arc<[Arc<str>]> {
    crate::plugins::host::record_live_auth_disk_read();
    match crate::ai::providers::load_pi_environment(Some(cwd)) {
        Ok(environment) => registry
            .providers()
            .iter()
            .filter(|provider| {
                crate::ai::providers::has_configured_auth(provider.id(), &environment.api_keys)
            })
            .map(|provider| Arc::<str>::from(provider.id()))
            .collect::<Vec<_>>()
            .into(),
        Err(_) => Arc::from(Vec::new()),
    }
}

impl ProviderService for LiveProviderService {
    fn register_auth_hooks(&self, provider_id: &str, hooks: Arc<dyn ProviderAuthHooks>) {
        // Process-wide table (auth entry points run outside a session) plus a
        // per-binding ownership record for teardown. Last registration wins.
        provider_auth_hook_registry().register(provider_id, Arc::clone(&hooks));
        self.registered_auth_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider_id.to_owned(), hooks);
    }

    fn unregister_auth_hooks(&self, provider_id: &str) {
        let owned = self
            .registered_auth_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider_id);
        // Last-wins: a newer binding may have replaced this service's
        // registration. Remove the process-wide entry only while this
        // service still owns it, so unregistering here can never tear down a
        // newer session's hook.
        if let Some(hooks) = owned {
            let _ = provider_auth_hook_registry().unregister_if(provider_id, &hooks);
        }
    }

    fn auth_hooks(&self, provider_id: &str) -> Option<Arc<dyn ProviderAuthHooks>> {
        provider_auth_hook_registry().get(provider_id)
    }

    fn physical_model(&self, provider: &str, id: &str) -> Option<Arc<Model>> {
        // The registry resolves virtual -> physical and returns the typed
        // model; one `Arc` wraps it for the ABI.
        self.state
            .registry
            .get_physical_model(provider, id)
            .map(Arc::new)
    }

    fn has_auth(&self, provider: &str) -> bool {
        let authenticated = self.authenticated_providers();
        authenticated.is_empty() || authenticated.iter().any(|id| id.as_ref() == provider)
    }

    fn mcp_servers(&self) -> Arc<[RegisteredMcpServer]> {
        #[cfg(feature = "mcp")]
        {
            let Some(host) = self.state.host() else {
                return Arc::from(Vec::new());
            };
            let Some(manager) = host.mcp_manager() else {
                return Arc::from(Vec::new());
            };
            // Extension registrations only (upstream `getMcpServers`); the
            // config is inherently JSON and crosses as `Arc<JsonValue>`.
            let servers: Vec<RegisteredMcpServer> = manager
                .registry()
                .list()
                .into_iter()
                .filter_map(|entry| {
                    let path = match &entry.source {
                        crate::mcp::registry::McpServerSource::Extension(path) => path,
                        _ => return None,
                    };
                    Some(RegisteredMcpServer {
                        name: Arc::from(entry.name.as_str()),
                        config: Arc::new(
                            serde_json::to_value(&entry.config).unwrap_or(JsonValue::Null),
                        ),
                        extension_path: Some(Arc::from(Path::new(path))),
                    })
                })
                .collect();
            Arc::from(servers)
        }
        #[cfg(not(feature = "mcp"))]
        {
            Arc::from(Vec::new())
        }
    }
}

impl Drop for LiveProviderService {
    fn drop(&mut self) {
        // Session/plugin-context teardown: remove only the hooks this binding
        // still owns. A later session that registered the same provider id
        // (last-wins) keeps its hook.
        let owned = std::mem::take(
            &mut *self
                .registered_auth_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (provider_id, hooks) in owned {
            let _ = provider_auth_hook_registry().unregister_if(&provider_id, &hooks);
        }
    }
}

// ---------------------------------------------------------------------------
// FlagService
// ---------------------------------------------------------------------------

struct LiveFlagService {
    state: Arc<LiveSessionState>,
}

impl FlagService for LiveFlagService {
    fn get(&self, name: &str) -> Option<FlagValue> {
        // A JS-registered flag carries the effective CLI value in the runtime
        // store (`CLI > default`); it is already typed.
        if let Some(js) = &self.state.js {
            if let Some(value) = js.get_flag(name) {
                return Some(value);
            }
        }
        // Native registrations: first flag def in plugin order, then the CLI
        // override collected by the host's argument preprocessing (the same
        // `--flag[=value]` map the JS loader applies).
        let host = self.state.host()?;
        let registry = host.registry();
        let def = registry
            .plugins()
            .iter()
            .flat_map(|plugin| plugin.registrations.flags.iter())
            .find(|flag| flag.name == name)?;
        if let Some(raw) = crate::coding_agent::cli::collected_unknown_flags().get(name) {
            if let Some(value) = parse_flag_value(&def.flag_type, raw) {
                return Some(value);
            }
        }
        def.default.clone()
    }
}

/// Parse one CLI `--flag[=value]` argument against the registered flag type
/// (the native twin of the extension loader's parser).
fn parse_flag_value(flag_type: &str, raw: &str) -> Option<FlagValue> {
    match flag_type {
        "boolean" => match raw {
            "" | "true" => Some(FlagValue::Bool(true)),
            "false" => Some(FlagValue::Bool(false)),
            _ => None,
        },
        _ if raw.is_empty() => None,
        _ => Some(FlagValue::Str(raw.to_owned())),
    }
}

// ---------------------------------------------------------------------------
// RendererService / CompletionService (native registries shared per host)
// ---------------------------------------------------------------------------

/// One host's native renderer + completion registries.
///
/// The registries are host-scoped, not binding-scoped: [`shared_native_registries`]
/// keeps one weak cell per `PluginHost` address, so calling `bind_context`
/// again for the same host (the documented invalidation seam) reuses the
/// registrations instead of dropping them. A dropped host removes the cell on
/// the next lookup (no strong reference is held here, so the session/host
/// cycle rule stays intact).
#[derive(Default)]
struct NativeRegistries {
    message_renderers: RwLock<HashMap<String, Arc<dyn MessageRenderer>>>,
    entry_renderers: RwLock<HashMap<String, Arc<dyn EntryRenderer>>>,
    markdown_transformers: RwLock<Vec<Arc<dyn MarkdownTransformer>>>,
    tool_renderers: RwLock<HashMap<String, Vec<Arc<dyn ToolRendererResolver>>>>,
    tool_fallback_renderers: RwLock<Vec<Arc<dyn ToolRendererResolver>>>,
    argument_completions: RwLock<HashMap<String, Arc<dyn ArgumentCompletions>>>,
    autocomplete_providers: RwLock<Vec<Arc<dyn AutocompleteProvider>>>,
    /// Native factory widgets in registration order (a key keeps its position
    /// on re-registration, matching the TUI cache's insertion order). The
    /// host-scoped cell is shared by `LiveUiService` (writes) and
    /// `NativeRendererService` (reads), so the TUI resolves the exact factory
    /// the plugin assigned.
    widget_factories: RwLock<Vec<(String, NativeWidgetFactory)>>,
    /// The native footer factory slot (`None` = no native registration).
    footer_factory: RwLock<Option<Arc<dyn BarFactory>>>,
    /// The native header factory slot (`None` = no native registration).
    header_factory: RwLock<Option<Arc<dyn BarFactory>>>,
    /// Native custom-overlay registrations awaiting their `DialogRequest`
    /// (`ctx.ui.custom`): dialog id → component. `LiveUiService::custom`
    /// writes one; the TUI takes it when it admits the request. One entry per
    /// in-flight request — no second overlay store lives in the TUI.
    overlay_components: RwLock<HashMap<u64, Arc<dyn OverlayComponent>>>,
}

/// One native widget factory registration: the factory plus its placement.
struct NativeWidgetFactory {
    factory: Arc<dyn WidgetFactory>,
    placement: WidgetPlacement,
}

impl NativeRegistries {
    /// Register (or replace, keeping its slot position) one native widget
    /// factory. Last registration per key wins.
    fn set_widget(&self, key: &str, factory: Arc<dyn WidgetFactory>, placement: WidgetPlacement) {
        let mut widgets = self
            .widget_factories
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, slot)) = widgets.iter_mut().find(|(slot, _)| slot == key) {
            *slot = NativeWidgetFactory { factory, placement };
        } else {
            widgets.push((key.to_owned(), NativeWidgetFactory { factory, placement }));
        }
    }

    /// Retire a native widget factory. Returns whether the native face owned
    /// the key (the caller uses that to decide between the JS fallback and the
    /// widget removal notification).
    fn remove_widget(&self, key: &str) -> bool {
        let mut widgets = self
            .widget_factories
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = widgets.len();
        widgets.retain(|(slot, _)| slot != key);
        widgets.len() != before
    }

    /// Install/clear the native footer slot. Returns whether a native
    /// registration was present before the call.
    fn set_footer(&self, factory: Option<Arc<dyn BarFactory>>) -> bool {
        let mut slot = self
            .footer_factory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_registered = slot.is_some();
        *slot = factory;
        was_registered
    }

    /// Install/clear the native header slot. Returns whether a native
    /// registration was present before the call.
    fn set_header(&self, factory: Option<Arc<dyn BarFactory>>) -> bool {
        let mut slot = self
            .header_factory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_registered = slot.is_some();
        *slot = factory;
        was_registered
    }

    fn has_widgets(&self) -> bool {
        !self
            .widget_factories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn has_widget(&self, key: &str) -> bool {
        self.widget_factories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(slot, _)| slot == key)
    }

    fn widget_placement(&self, key: &str) -> Option<WidgetPlacement> {
        self.widget_factories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(slot, _)| slot == key)
            .map(|(_, registration)| registration.placement)
    }

    fn has_footer(&self) -> bool {
        self.footer_factory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn has_header(&self) -> bool {
        self.header_factory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Store the component a native `ctx.ui.custom` request will show.
    fn set_overlay_component(&self, id: u64, component: Arc<dyn OverlayComponent>) {
        self.overlay_components
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, component);
    }

    /// Take the component for `id` (one registration, one overlay).
    fn take_overlay_component(&self, id: u64) -> Option<Arc<dyn OverlayComponent>> {
        self.overlay_components
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
    }

    /// Drop a registration that never surfaced (cancelled/timeout), disposing
    /// the component exactly once.
    fn remove_overlay_component(&self, id: u64) {
        let component = self.take_overlay_component(id);
        if let Some(component) = component {
            catch_native_callback("custom overlay component", || component.dispose());
        }
    }

    /// Render one native widget factory. `None` = not registered or the
    /// factory panicked (isolated; the caller falls back to the JS face).
    fn render_widget(&self, key: &str, context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        let factory = self
            .widget_factories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(slot, _)| slot == key)
            .map(|(_, registration)| Arc::clone(&registration.factory))?;
        catch_native_callback("widget factory", || factory.render(context))
    }

    fn render_footer(&self, context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        let factory = self
            .footer_factory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        catch_native_callback("footer factory", || factory.render(context))
    }

    fn render_header(&self, context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        let factory = self
            .header_factory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        catch_native_callback("header factory", || factory.render(context))
    }
}

/// Weak per-host registry cells: `PluginHost` address → registries. Pruned on
/// every lookup; no strong references, so a dropped host is collectable.
type NativeRegistryCells = Mutex<HashMap<usize, Weak<NativeRegistries>>>;

fn native_registry_cells() -> &'static NativeRegistryCells {
    static CELLS: OnceLock<NativeRegistryCells> = OnceLock::new();
    CELLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get (or create) the native registries for one host.
fn shared_native_registries(host: &Arc<PluginHost>) -> Arc<NativeRegistries> {
    let key = Arc::as_ptr(host) as usize;
    let mut cells = native_registry_cells()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cells.retain(|_, weak| weak.strong_count() > 0);
    if let Some(registries) = cells.get(&key).and_then(Weak::upgrade) {
        return registries;
    }
    let registries = Arc::new(NativeRegistries::default());
    cells.insert(key, Arc::downgrade(&registries));
    registries
}

/// Run one native plugin callback with panic isolation. A panicking renderer
/// or completion provider is logged and reported as "unhandled"/empty, so the
/// caller falls back to the JS/built-in path instead of unwinding through the
/// TUI frame or the completion dispatcher.
fn catch_native_callback<T>(what: &'static str, callback: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::error!(
                callback = what,
                "native plugin callback panicked; isolating"
            );
            None
        }
    }
}

/// Await one native completion future with panic isolation.
async fn catch_native_completion(
    future: ServiceFuture<'static, Vec<AutocompleteItem>>,
) -> Vec<AutocompleteItem> {
    use futures_util::FutureExt;
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(items) => items,
        Err(_) => {
            tracing::error!("native completion provider panicked; isolating");
            Vec::new()
        }
    }
}

/// Append `next` suggestions, keeping the first item per `value` (the same
/// prefix dedupe the TUI applies when it merges the JS chain).
fn merge_native_items(
    mut items: Vec<AutocompleteItem>,
    next: Vec<AutocompleteItem>,
) -> Vec<AutocompleteItem> {
    let mut seen: std::collections::HashSet<String> =
        items.iter().map(|item| item.value.clone()).collect();
    for item in next {
        if seen.insert(item.value.clone()) {
            items.push(item);
        }
    }
    items
}

/// Whether a tool's extra metadata declares a live JS renderer (upstream
/// `ToolDefinition.renderCall`/`renderResult`).
fn tool_extra_has_renderers(extra: JsonValue) -> bool {
    extra.get("renderCall").and_then(JsonValue::as_bool) == Some(true)
        || extra.get("renderResult").and_then(JsonValue::as_bool) == Some(true)
}

/// Whether the render request carries no settled output yet (the tool block is
/// still on its call half).
fn tool_render_result_is_empty(result: &JsonValue) -> bool {
    result
        .get("output")
        .and_then(JsonValue::as_str)
        .is_none_or(str::is_empty)
}

/// Live native renderer registry.
struct NativeRendererService {
    registries: Arc<NativeRegistries>,
    /// The JS runtime face: a tool's own `renderCall`/`renderResult` live in
    /// QuickJS and are dispatched through the actor mailbox, not the native
    /// registries (upstream `ToolDefinition.renderCall`/`renderResult`).
    js: Option<ExtensionHandle>,
}

impl RendererService for NativeRendererService {
    fn register_message(&self, custom_type: &str, renderer: Arc<dyn MessageRenderer>) {
        // First registration per custom type wins (the JS registry's rule).
        self.registries
            .message_renderers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(custom_type.to_owned())
            .or_insert(renderer);
    }

    fn register_entry(&self, custom_type: &str, renderer: Arc<dyn EntryRenderer>) {
        self.registries
            .entry_renderers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(custom_type.to_owned())
            .or_insert(renderer);
    }

    fn register_markdown(&self, transformer: Arc<dyn MarkdownTransformer>) {
        self.registries
            .markdown_transformers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(transformer);
    }

    fn register_tool(&self, tool_name: Option<&str>, resolver: Arc<dyn ToolRendererResolver>) {
        match tool_name {
            Some(tool_name) => self
                .registries
                .tool_renderers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(tool_name.to_owned())
                .or_default()
                .push(resolver),
            None => self
                .registries
                .tool_fallback_renderers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(resolver),
        }
    }

    fn has(&self, kind: RendererKind, key: &str) -> bool {
        match kind {
            RendererKind::Message => self
                .registries
                .message_renderers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(key),
            RendererKind::Entry => self
                .registries
                .entry_renderers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(key),
            RendererKind::Markdown => !self
                .registries
                .markdown_transformers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            RendererKind::Tool => {
                self.registries
                    .tool_renderers
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(key)
                    // A tool's own renderCall/renderResult (JS face) also
                    // makes the slot renderable.
                    || self
                        .js
                        .as_ref()
                        .and_then(|handle| handle.tool_extra(key))
                        .is_some_and(tool_extra_has_renderers)
                    || !self
                        .registries
                        .tool_fallback_renderers
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_empty()
            }
            // Native factory slots written by `LiveUiService`; the TUI
            // resolves them through the factory renderer below, before the JS
            // `__pir_callRenderer` bridge.
            RendererKind::Widget => self.registries.has_widget(key),
            RendererKind::Footer => self.registries.has_footer(),
            RendererKind::Header => self.registries.has_header(),
        }
    }

    fn has_native_factories(&self) -> bool {
        self.registries.has_widgets()
            || self.registries.has_footer()
            || self.registries.has_header()
    }

    // -- native custom overlay components (`ctx.ui.custom`) -------------------
    //
    // The TUI resolves `DialogRequest { kind: "custom", options.component }`
    // by taking the component here when the key names a native registration
    // (`options.component` = `native:<dialog id>`); otherwise the request is
    // the JS face's and the prelude's component key is used. One registry,
    // one lookup, whichever face opened the overlay.
    fn has_overlay_component(&self, id: u64) -> bool {
        self.registries
            .overlay_components
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id)
    }

    fn take_overlay_component(&self, id: u64) -> Option<Arc<dyn OverlayComponent>> {
        self.registries.take_overlay_component(id)
    }

    fn widget_placement(&self, key: &str) -> Option<WidgetPlacement> {
        self.registries.widget_placement(key)
    }

    fn render_widget(&self, key: &str, context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        self.registries.render_widget(key, context)
    }

    fn render_footer(&self, context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        self.registries.render_footer(context)
    }

    fn render_header(&self, context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        self.registries.render_header(context)
    }

    fn render_message(
        &self,
        custom_type: &str,
        message: &JsonValue,
        width: usize,
    ) -> RendererOutcome {
        let renderer = self
            .registries
            .message_renderers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(custom_type)
            .cloned();
        match renderer {
            Some(renderer) => {
                catch_native_callback("message renderer", || renderer.render(message, width))
                    .unwrap_or_default()
            }
            None => RendererOutcome::default(),
        }
    }

    fn render_entry(&self, custom_type: &str, entry: &JsonValue, width: usize) -> RendererOutcome {
        let renderer = self
            .registries
            .entry_renderers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(custom_type)
            .cloned();
        match renderer {
            Some(renderer) => {
                catch_native_callback("entry renderer", || renderer.render(entry, width))
                    .unwrap_or_default()
            }
            None => RendererOutcome::default(),
        }
    }

    fn transform_markdown(&self, markdown: &str) -> Option<String> {
        let transformers: Vec<Arc<dyn MarkdownTransformer>> = self
            .registries
            .markdown_transformers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if transformers.is_empty() {
            return None;
        }
        let mut text = markdown.to_string();
        let mut changed = false;
        for transformer in transformers {
            // `None` keeps the current text; `Some` feeds the next
            // transformer in registration order.
            if let Some(Some(next)) =
                catch_native_callback("markdown transformer", || transformer.transform(&text))
            {
                text = next;
                changed = true;
            }
        }
        changed.then_some(text)
    }

    fn markdown_transformer_count(&self) -> usize {
        self.registries
            .markdown_transformers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn render_tool(&self, request: &ToolRenderRequest<'_>) -> RendererOutcome {
        self.render_tool_with_shell(request).outcome
    }

    fn render_tool_with_shell(&self, request: &ToolRenderRequest<'_>) -> RendererCallOutcome {
        let mut resolvers = self
            .registries
            .tool_renderers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(request.tool_name)
            .cloned()
            .unwrap_or_default();
        resolvers.extend(
            self.registries
                .tool_fallback_renderers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        for resolver in resolvers {
            let outcome = match catch_native_callback("tool renderer", || {
                resolver.render_with_shell(request)
            }) {
                Some(outcome) => outcome,
                None => continue,
            };
            if !outcome.is_unhandled() {
                return outcome;
            }
        }
        // The tool's own `renderCall`/`renderResult` (JS face). Native
        // resolvers run first; an unhandled outcome delegates here exactly
        // like upstream's renderer chain. The split between the two kinds is
        // observable only through the result payload: a settled block renders
        // `result`, a running call renders `call`.
        if let Some(js) = &self.js {
            if let Some(extra) = js.tool_extra(request.tool_name) {
                let has_call = extra.get("renderCall").and_then(JsonValue::as_bool) == Some(true);
                let has_result =
                    extra.get("renderResult").and_then(JsonValue::as_bool) == Some(true);
                let settled = !tool_render_result_is_empty(request.result);
                let render_kind = match (has_result, has_call) {
                    (true, _) if settled => "result",
                    (true, false) => "result",
                    (false, true) => "call",
                    _ => return RendererCallOutcome::default(),
                };
                let payload = serde_json::json!({
                    "args": request.args,
                    "result": request.result,
                    "isError": request.is_error,
                    "width": request.width,
                });
                if let Ok(outcome) = js.call_tool_renderer_sync_full(
                    request.tool_name,
                    render_kind,
                    &payload,
                    // Matches the TUI's bounded renderer call; the actor
                    // mailbox enforces its own long-call budget.
                    std::time::Duration::from_secs(2),
                ) {
                    if !outcome.is_unhandled() {
                        return outcome;
                    }
                }
            }
        }
        RendererCallOutcome::default()
    }
}

/// Live native completion registry.
struct NativeCompletionService {
    registries: Arc<NativeRegistries>,
}

impl CompletionService for NativeCompletionService {
    fn register_argument(&self, command: &str, provider: Arc<dyn ArgumentCompletions>) {
        // First registration per command wins (the JS lookup's rule: first
        // extension in load order that registered the command).
        self.registries
            .argument_completions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(command.to_owned())
            .or_insert(provider);
    }

    fn register_provider(&self, provider: Arc<dyn AutocompleteProvider>) {
        self.registries
            .autocomplete_providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(provider);
    }

    fn has_argument_provider(&self, command: &str) -> bool {
        self.registries
            .argument_completions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(command)
    }

    fn has_providers(&self) -> bool {
        !self
            .registries
            .autocomplete_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn argument_completions(
        &self,
        command: &str,
        prefix: &str,
    ) -> ServiceFuture<'static, Vec<AutocompleteItem>> {
        let provider = self
            .registries
            .argument_completions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(command)
            .cloned();
        let prefix = prefix.to_owned();
        Box::pin(async move {
            match provider {
                Some(provider) => catch_native_completion(provider.complete(&prefix)).await,
                None => Vec::new(),
            }
        })
    }

    fn suggest(
        &self,
        context: &CompletionContext,
    ) -> ServiceFuture<'static, Vec<AutocompleteItem>> {
        let providers: Vec<Arc<dyn AutocompleteProvider>> = self
            .registries
            .autocomplete_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let context = context.clone();
        Box::pin(async move {
            let mut merged = Vec::new();
            // Registration order; one failing/panicking provider contributes
            // nothing and never skips the providers after it.
            for provider in providers {
                let items = catch_native_completion(provider.suggest(&context)).await;
                merged = merge_native_items(merged, items);
            }
            merged
        })
    }
}

// ---------------------------------------------------------------------------
// StatusService
// ---------------------------------------------------------------------------

/// Live status cells: one insertion-ordered map, one bounded lock write per
/// `set_status`, and an `Arc` snapshot per read. A repeated `set_status` for
/// one key overwrites in place (latest set wins) and keeps its position, so a
/// footer renders a stable order.
///
/// Lock discipline: `cells` is a leaf lock. Every method holds it only for the
/// duration of a `Vec` update/scan and never calls plugin or host code while
/// it is held, so a status consumer (including a plugin callback that polls
/// [`StatusService::statuses`] and then writes a cell) cannot re-enter the
/// lock or invert an order with another lock. [`PluginHost::statuses`] reads
/// this map once after collecting the polled plugin entries; there is no
/// callback path inside the service to deadlock with.
#[derive(Default)]
struct NativeStatusService {
    cells: Mutex<Vec<StatusEntry>>,
}

impl StatusService for NativeStatusService {
    fn set_status(&self, key: &str, text: Option<&str>) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cells.iter().position(|(cell, _)| cell.as_ref() == key) {
            Some(index) => match text {
                Some(text) => cells[index].1 = Arc::from(text),
                None => {
                    cells.remove(index);
                }
            },
            None => {
                if let Some(text) = text {
                    cells.push((Arc::from(key), Arc::from(text)));
                }
            }
        }
    }

    fn statuses(&self) -> StatusSnapshot {
        Arc::from(
            self.cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

    fn status(&self, key: &str) -> Option<Arc<str>> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(cell, _)| cell.as_ref() == key)
            .map(|(_, text)| Arc::clone(text))
    }
}

// ---------------------------------------------------------------------------
// EventService (`pi.events` bus)
// ---------------------------------------------------------------------------

/// The `pi.events` bus: synchronous, per-channel subscriber lists.
///
/// Ordering contract: callbacks run inline on the emitting thread in
/// subscription order (FIFO per subscriber); there is no ordering guarantee
/// across subscribers or channels beyond that. The fan-out is bounded at
/// [`MAX_EVENT_SUBSCRIBERS_PER_CHANNEL`] subscribers per channel.
///
/// Panic isolation: each callback runs inside
/// [`std::panic::catch_unwind`], so a panicking subscriber is logged and the
/// remaining subscribers still receive the event. The subscriber list is
/// cloned and the channel lock released before any callback runs, so a panic
/// can neither poison the mutex nor unwind into the emitting plugin/tool.
#[derive(Default)]
struct NativeEventBus {
    channels: EventChannels,
    next_subscription: AtomicU64,
}

impl EventService for NativeEventBus {
    fn emit(&self, channel: &str, data: JsonValue) {
        // Clone the callback `Arc`s under the lock, release it, then invoke:
        // a callback may subscribe/emit/cancel without deadlocking, and a
        // panic cannot poison the map's mutex because it is not held here.
        let callbacks: Vec<EventCallback> = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(channel)
            .map(|subscribers| {
                subscribers
                    .iter()
                    .map(|(_, callback)| Arc::clone(callback))
                    .collect()
            })
            .unwrap_or_default();
        let data = &data;
        for callback in callbacks {
            // A subscriber is plugin-authored native code: isolate its panic
            // so it neither unwinds into the caller nor skips later
            // subscribers. Delivery order is unchanged.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(data))).is_err() {
                tracing::error!(
                    channel,
                    "plugin event-bus subscriber panicked; continuing delivery"
                );
            }
        }
    }

    fn subscribe(&self, channel: &str, callback: EventCallback) -> Subscription {
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        let channels = Arc::clone(&self.channels);
        {
            let mut channels_guard = channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let subscribers = channels_guard.entry(channel.to_owned()).or_default();
            if subscribers.len() >= MAX_EVENT_SUBSCRIBERS_PER_CHANNEL {
                // Bounded fan-out: refuse instead of growing without limit.
                return Subscription::noop();
            }
            subscribers.push((id, callback));
        }
        let channel = channel.to_owned();
        Subscription::new(move || {
            let mut channels_guard = channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(subscribers) = channels_guard.get_mut(&channel) {
                subscribers.retain(|(subscription, _)| *subscription != id);
                if subscribers.is_empty() {
                    channels_guard.remove(&channel);
                }
            }
        })
    }
}
