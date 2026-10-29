//! [`PluginContext`]: the typed host-service handoff shared by the Rust
//! plugin face and (through the host-function adapters) the JS face.
//!
//! A context is a cheap-to-clone bundle of `Arc<dyn Service>` handles plus the
//! identity of the plugin it was handed to. It is **additive**: a host that
//! binds no context keeps exercising exactly the pre-service behavior, and
//! [`AgentPlugin::on_context`](crate::plugins::plugin::AgentPlugin::on_context)
//! is a default no-op.
//!
//! # Lifecycle
//!
//! 1. The host builds a [`PluginServices`] set
//!    ([`super::services::PluginServices`]) and calls
//!    [`PluginHost::bind_context`](crate::plugins::PluginHost::bind_context).
//! 2. `bind_context` bumps the host's generation cell, stamps the context
//!    with the new epoch, hands every plugin a per-plugin identity view via
//!    `on_context`, and stores the bound context.
//! 3. A plugin stores its context clone and uses it for the rest of its
//!    activation. `register()` runs before the bind, so a plugin that needs
//!    the context during registration stores it in `on_context` and reads it
//!    from `on_event` / tool bodies.
//!
//! # Epoch / `/reload` rule
//!
//! `PluginContext` stores the `epoch: u64` it was bound at and an `Arc` to
//! the host's live generation cell. `bind_context` **always** increments that
//! cell and stamps the new context with the new value. Consequences:
//!
//! - Every context bound before the call — including clones a plugin cached —
//!   reports [`PluginContext::is_active`] `false`.
//! - [`PluginContext::check_active`] returns
//!   [`PluginServiceError::Stale`] carrying both epochs, so a stale caller can
//!   distinguish "the host moved on" from "the capability is absent".
//! - The rule is one line for hosts: on `/reload`, `newSession`, `fork`,
//!   `switchSession`, or any session replacement, build a fresh
//!   `PluginServices` and call `bind_context` again. That call is the single
//!   invalidation point; there is no per-service generation bookkeeping.
//!
//! Reads on a stale context are still safe (they return the last snapshot the
//! service holds). Mutating calls should gate on `check_active` before
//! touching host state; the helper [`PluginContext::require_session`] etc.
//! combine both checks.
//!
//! # Performance seam
//!
//! Accessors are plain `Option` field reads with no allocation. The `Arc`
//! clone is paid only by the service method the caller invokes. The
//! `#[doc(hidden)]` [`PluginContext::measure_ns`] helper gives the later perf
//! lane a dependency-free way to measure an accessor closure (targets:
//! status write/read ≤ 200 ns, entries read ≤ 100 ns in a release build).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use super::services::{PluginServiceError, ServiceResult};

use super::services::{
    CancellationHandle, CompletionService, EventService, FlagService, MessageService,
    PluginServices, ProviderService, RendererService, SessionService, StatusService, ToolService,
    UiService,
};

/// Identity of the plugin a context was handed to.
///
/// `path`/`name` mirror [`PluginInfo`](crate::plugins::source::PluginInfo);
/// `order` is the plugin's index in the host's ordered dispatch list (used by
/// renderer/completion ordering and diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginIdentity {
    path: Arc<str>,
    name: Arc<str>,
    builtin: Option<Arc<str>>,
    order: usize,
}

impl PluginIdentity {
    /// Identity for a context that is not (yet) bound to a host plugin.
    pub fn host() -> Self {
        Self::new("<host>", "<host>", 0)
    }

    /// Build an identity from its path/name/order.
    pub fn new(path: impl Into<Arc<str>>, name: impl Into<Arc<str>>, order: usize) -> Self {
        let path = path.into();
        let builtin = crate::plugins::source::builtin_name(path.as_ref()).map(Arc::from);
        Self {
            path,
            name: name.into(),
            builtin,
            order,
        }
    }

    /// Derive the identity of one plugin in the host's ordered list.
    pub fn from_plugin(info: &crate::plugins::source::PluginInfo, order: usize) -> Self {
        Self::new(info.path.as_str(), info.name.as_str(), order)
    }

    /// Display path (`builtin:<name>` or the extension path).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Plugin name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The compiled-builtin name, when the plugin is a `builtin:<name>`.
    pub fn builtin(&self) -> Option<&str> {
        self.builtin.as_deref()
    }

    /// Index in the host's ordered plugin list.
    pub fn order(&self) -> usize {
        self.order
    }
}

/// Scope of one Rust tool invocation (`ctx.tools` / `ctx.executeTool`).
///
/// `ToolService::execute_tool` needs the calling tool's call id to mint
/// `<caller>/<n>` child ids; hosts attach the scope when they run a Rust tool
/// and pass the scoped context through the tool body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallScope {
    pub tool_call_id: Arc<str>,
    pub tool_name: Arc<str>,
    pub parent_call_id: Option<Arc<str>>,
}

impl ToolCallScope {
    /// A top-level tool call scope.
    pub fn new(tool_call_id: impl Into<Arc<str>>, tool_name: impl Into<Arc<str>>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            parent_call_id: None,
        }
    }

    /// Attach the parent call id for nested calls.
    pub fn with_parent(mut self, parent: impl Into<Arc<str>>) -> Self {
        self.parent_call_id = Some(parent.into());
        self
    }
}

/// The typed service context handed to a plugin for one activation.
///
/// Cloning is cheap (every field is an `Arc` or a scalar). See the module
/// docs for the epoch contract and the performance rules.
#[derive(Clone)]
pub struct PluginContext {
    pub(crate) session: Option<Arc<dyn SessionService>>,
    pub(crate) messages: Option<Arc<dyn MessageService>>,
    pub(crate) tools: Option<Arc<dyn ToolService>>,
    pub(crate) ui: Option<Arc<dyn UiService>>,
    pub(crate) providers: Option<Arc<dyn ProviderService>>,
    pub(crate) renderers: Option<Arc<dyn RendererService>>,
    pub(crate) completions: Option<Arc<dyn CompletionService>>,
    pub(crate) events: Option<Arc<dyn EventService>>,
    pub(crate) status: Option<Arc<dyn StatusService>>,
    pub(crate) flags: Option<Arc<dyn FlagService>>,
    identity: Arc<PluginIdentity>,
    epoch: u64,
    epoch_cell: Arc<AtomicU64>,
    tool_scope: Option<Arc<ToolCallScope>>,
}

impl PluginContext {
    /// The identity of the plugin this context was handed to.
    pub fn identity(&self) -> &PluginIdentity {
        &self.identity
    }

    /// The generation this context was bound at.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The host's current generation (the live epoch cell).
    pub fn live_epoch(&self) -> u64 {
        self.epoch_cell.load(Ordering::Acquire)
    }

    /// Whether the host is still at the generation this context was bound at.
    ///
    /// Cheap enough for hot paths: one relaxed-load-equivalent atomic read
    /// and an integer comparison.
    pub fn is_active(&self) -> bool {
        self.epoch == self.live_epoch()
    }

    /// Fail with [`PluginServiceError::Stale`] when the host has rebound a
    /// newer context (for example after `/reload`).
    pub fn check_active(&self) -> ServiceResult<()> {
        let current = self.live_epoch();
        if current == self.epoch {
            Ok(())
        } else {
            Err(PluginServiceError::stale(self.epoch, current))
        }
    }

    // -- raw accessors (zero allocation) -----------------------------------

    /// The session service, when bound.
    pub fn session(&self) -> Option<&dyn SessionService> {
        self.session.as_deref()
    }

    /// The message service, when bound.
    pub fn messages(&self) -> Option<&dyn MessageService> {
        self.messages.as_deref()
    }

    /// The tool service, when bound.
    pub fn tools(&self) -> Option<&dyn ToolService> {
        self.tools.as_deref()
    }

    /// The UI service, when bound.
    pub fn ui(&self) -> Option<&dyn UiService> {
        self.ui.as_deref()
    }

    /// The provider service, when bound.
    pub fn providers(&self) -> Option<&dyn ProviderService> {
        self.providers.as_deref()
    }

    /// The renderer registration service, when bound.
    pub fn renderers(&self) -> Option<&dyn RendererService> {
        self.renderers.as_deref()
    }

    /// The completion service, when bound.
    pub fn completions(&self) -> Option<&dyn CompletionService> {
        self.completions.as_deref()
    }

    /// The extension event bus, when bound.
    pub fn events(&self) -> Option<&dyn EventService> {
        self.events.as_deref()
    }

    /// The native status-cell service, when bound.
    pub fn status(&self) -> Option<&dyn StatusService> {
        self.status.as_deref()
    }

    /// The flag service, when bound.
    pub fn flags(&self) -> Option<&dyn FlagService> {
        self.flags.as_deref()
    }

    /// Arc accessors for callers that must move the service into a
    /// `'static` future or task. The clone is the only cost.
    pub fn session_arc(&self) -> Option<Arc<dyn SessionService>> {
        self.session.clone()
    }

    /// See [`Self::session_arc`].
    pub fn messages_arc(&self) -> Option<Arc<dyn MessageService>> {
        self.messages.clone()
    }

    /// See [`Self::session_arc`].
    pub fn tools_arc(&self) -> Option<Arc<dyn ToolService>> {
        self.tools.clone()
    }

    /// See [`Self::session_arc`].
    pub fn ui_arc(&self) -> Option<Arc<dyn UiService>> {
        self.ui.clone()
    }

    /// See [`Self::session_arc`].
    pub fn providers_arc(&self) -> Option<Arc<dyn ProviderService>> {
        self.providers.clone()
    }

    /// See [`Self::session_arc`].
    pub fn renderers_arc(&self) -> Option<Arc<dyn RendererService>> {
        self.renderers.clone()
    }

    /// See [`Self::session_arc`].
    pub fn completions_arc(&self) -> Option<Arc<dyn CompletionService>> {
        self.completions.clone()
    }

    /// See [`Self::session_arc`].
    pub fn events_arc(&self) -> Option<Arc<dyn EventService>> {
        self.events.clone()
    }

    /// See [`Self::session_arc`].
    pub fn status_arc(&self) -> Option<Arc<dyn StatusService>> {
        self.status.clone()
    }

    /// See [`Self::session_arc`].
    pub fn flags_arc(&self) -> Option<Arc<dyn FlagService>> {
        self.flags.clone()
    }

    // -- require helpers (epoch check + presence check) ---------------------

    /// Session service, after verifying the context is not stale.
    pub fn require_session(&self) -> ServiceResult<&dyn SessionService> {
        self.check_active()?;
        self.session
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("session"))
    }

    /// Message service, after verifying the context is not stale.
    pub fn require_messages(&self) -> ServiceResult<&dyn MessageService> {
        self.check_active()?;
        self.messages
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("messages"))
    }

    /// Tool service, after verifying the context is not stale.
    pub fn require_tools(&self) -> ServiceResult<&dyn ToolService> {
        self.check_active()?;
        self.tools
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("tools"))
    }

    /// UI service, after verifying the context is not stale.
    pub fn require_ui(&self) -> ServiceResult<&dyn UiService> {
        self.check_active()?;
        self.ui
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("ui"))
    }

    /// Provider service, after verifying the context is not stale.
    pub fn require_providers(&self) -> ServiceResult<&dyn ProviderService> {
        self.check_active()?;
        self.providers
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("providers"))
    }

    /// Renderer service, after verifying the context is not stale.
    pub fn require_renderers(&self) -> ServiceResult<&dyn RendererService> {
        self.check_active()?;
        self.renderers
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("renderers"))
    }

    /// Completion service, after verifying the context is not stale.
    pub fn require_completions(&self) -> ServiceResult<&dyn CompletionService> {
        self.check_active()?;
        self.completions
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("completions"))
    }

    /// Event bus, after verifying the context is not stale.
    pub fn require_events(&self) -> ServiceResult<&dyn EventService> {
        self.check_active()?;
        self.events
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("events"))
    }

    /// Status service, after verifying the context is not stale.
    pub fn require_status(&self) -> ServiceResult<&dyn StatusService> {
        self.check_active()?;
        self.status
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("status"))
    }

    /// Flag service, after verifying the context is not stale.
    pub fn require_flags(&self) -> ServiceResult<&dyn FlagService> {
        self.check_active()?;
        self.flags
            .as_deref()
            .ok_or_else(|| PluginServiceError::unavailable("flags"))
    }

    /// Cancellation handle, after verifying the context is not stale.
    pub fn require_signal(&self) -> ServiceResult<CancellationHandle> {
        Ok(self.require_session()?.signal())
    }

    // -- scoped variants ----------------------------------------------------

    /// The tool-call scope attached to this context, when inside a Rust
    /// tool's run.
    pub fn tool_scope(&self) -> Option<&ToolCallScope> {
        self.tool_scope.as_deref()
    }

    /// A clone of this context scoped to one tool call.
    pub fn with_tool_scope(&self, scope: ToolCallScope) -> Self {
        let mut scoped = self.clone();
        scoped.tool_scope = Some(Arc::new(scope));
        scoped
    }

    /// A clone of this context with another plugin identity (host-internal
    /// per-plugin handoff).
    pub fn with_identity(&self, identity: PluginIdentity) -> Self {
        let mut scoped = self.clone();
        scoped.identity = Arc::new(identity);
        scoped
    }

    /// Re-stamp this context onto a host generation cell. Host-internal:
    /// [`PluginHost::bind_context`](crate::plugins::PluginHost::bind_context)
    /// is the only caller.
    pub(crate) fn with_epoch(mut self, epoch_cell: Arc<AtomicU64>, epoch: u64) -> Self {
        self.epoch_cell = epoch_cell;
        self.epoch = epoch;
        self
    }

    // -- introspection / perf seam -----------------------------------------

    /// How many services are bound. `#[doc(hidden)]` test/diagnostic seam.
    #[doc(hidden)]
    pub fn bound_services(&self) -> usize {
        [
            self.session.is_some(),
            self.messages.is_some(),
            self.tools.is_some(),
            self.ui.is_some(),
            self.providers.is_some(),
            self.renderers.is_some(),
            self.completions.is_some(),
            self.events.is_some(),
            self.status.is_some(),
            self.flags.is_some(),
        ]
        .into_iter()
        .filter(|bound| *bound)
        .count()
    }

    /// Measure an accessor closure in nanoseconds per call.
    ///
    /// `#[doc(hidden)]` micro-benchmark seam for the later perf lane (no
    /// criterion dependency in the contract test). `f` receives the context
    /// each iteration; the result is black-boxed so the reads are not folded
    /// away.
    #[doc(hidden)]
    pub fn measure_ns<T>(&self, iterations: u32, mut f: impl FnMut(&Self) -> T) -> u128 {
        let iterations = iterations.max(1);
        let start = std::time::Instant::now();
        let mut sink = None;
        for _ in 0..iterations {
            sink = Some(std::hint::black_box(f(self)));
        }
        std::hint::black_box(sink);
        start.elapsed().as_nanos() / u128::from(iterations)
    }
}

impl From<PluginServices> for PluginContext {
    fn from(services: PluginServices) -> Self {
        let epoch = services.epoch_cell.load(Ordering::Acquire);
        Self {
            session: services.session,
            messages: services.messages,
            tools: services.tools,
            ui: services.ui,
            providers: services.providers,
            renderers: services.renderers,
            completions: services.completions,
            events: services.events,
            status: services.status,
            flags: services.flags,
            identity: Arc::new(services.identity),
            epoch,
            epoch_cell: services.epoch_cell,
            tool_scope: None,
        }
    }
}

impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("identity", &self.identity)
            .field("epoch", &self.epoch)
            .field("live_epoch", &self.live_epoch())
            .field("bound_services", &self.bound_services())
            .field("tool_scope", &self.tool_scope)
            .finish()
    }
}
