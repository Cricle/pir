//! Native extension host interface: result/cancel-aware session replacement
//! and floating custom overlays.
//!
//! # Why this exists
//!
//! [`crate::plugins::plugin::services::SessionService`]'s `new_session` /
//! `fork` / `switch_session` methods return `()` and carry no cancellation
//! channel (the trait predates the host's session-replacement flow). They
//! remain documented no-ops by default, so a native plugin that calls them
//! cannot tell "replaced" from "unsupported". The `UiService::custom`
//! signature likewise carries only a deadline, so a native custom overlay can
//! only use the editor slot.
//!
//! [`NativeSessionHost`] is the honest native face for both:
//!
//! - session replacement returns a typed [`SessionReplacement`] (new session
//!   id/path/file, leaf, fork `selectedText`), observes a caller-supplied
//!   [`AbortSignal`] at defined checkpoints, and reports
//!   [`NativeHostError::Cancelled`] / `Unsupported` / `Failed` instead of a
//!   silent success;
//! - [`NativeSessionHost::capability`] answers whether this host can perform
//!   an action BEFORE it is attempted;
//! - [`NativeSessionHost::custom_overlay`] takes [`NativeOverlayOptions`] and
//!   a [`NativeOverlayHandle`], and rides the one
//!   `Notification::DialogRequest` pipeline (`overlay: true` +
//!   `overlayOptions`), so the TUI floats the component exactly like the JS
//!   face. [`NativeOverlayFactory`] additionally lets the EXISTING
//!   `UiService::custom(factory, options)` method float by carrying the same
//!   option bag on the factory (see `extensions::overlay`).
//!
//! # Reaching the host from a native plugin
//!
//! The plugin only holds a [`PluginContext`] (the service layer cannot be
//! extended in this lane). Every `Arc`-constructed session with attached
//! storage registers a `Weak` handle process-wide, keyed by its current
//! session id; [`native_session_host`] resolves it from the context's live
//! session id. A session that was built by value, has no storage, or whose
//! id changed without re-registration reports `Unsupported` with the exact
//! reason.
//!
//! # Cancellation contract
//!
//! `AbortSignal` is observed:
//!
//! - before any work starts;
//! - after the replacement storage has been prepared (new file / opened
//!   session / forked copy) and BEFORE the live session is swapped. A cancel
//!   at that point leaves the original session untouched and reports
//!   [`NativeHostError::Cancelled`];
//! - fork-at-entry checks before the branch is applied; once the extension
//!   `session_before_fork` dispatch has applied a branch, the honest outcome
//!   is `Ok` (the change happened).
//!
//! Once the swap begins it runs to completion (the caller observes an atomic
//! replacement, never a half-swapped session). Cancelling mid-swap is
//! intentionally not reported as `Cancelled`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value as JsonValue;

use crate::ai::types::AbortSignal;
use crate::extensions::overlay::{NativeOverlayHandle, NativeOverlayOptions, OverlayFactorySpec};
use crate::plugins::plugin::services::{
    ForkOptions, NewSessionOptions, OverlayComponent, OverlayComponentFactory, OverlayDone,
    PluginServiceError,
};
use crate::plugins::plugin::PluginContext;

use super::AgentSession;

/// Boxed async return of a native host call.
pub type NativeHostFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Which native session action a capability check describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSessionAction {
    NewSession,
    Fork,
    SwitchSession,
}

/// Whether one native host action is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeCapability {
    /// The live session can perform the action.
    Available,
    /// The action cannot be performed; `reason` is actionable (no storage,
    /// dropped session, no UI sink, ...).
    Unsupported { reason: String },
}

impl NativeCapability {
    /// Whether the action is available.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Typed native host failure (never a stringly-typed silent success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeHostError {
    /// The host cannot perform the action (no storage, no UI sink, stale or
    /// unregistered session).
    Unsupported { reason: String },
    /// The caller's cancellation signal fired at a cancellation checkpoint,
    /// or an extension handler refused the replacement.
    Cancelled { reason: String },
    /// The action ran and failed.
    Failed { message: String },
}

impl NativeHostError {
    fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for NativeHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { reason } => f.write_str(reason),
            Self::Cancelled { reason } => f.write_str(reason),
            Self::Failed { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for NativeHostError {}

/// The applied result of one native session replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReplacement {
    /// `true` for new-session / switch / fork-at-current (the storage slot
    /// was replaced). `false` for fork-at-entry, which branches the tree in
    /// place inside the same file.
    pub replaced_storage: bool,
    /// Session id after the operation.
    pub session_id: Option<String>,
    /// Planned/actual session file path.
    pub session_path: Option<PathBuf>,
    /// `None` for an in-memory manager (no persisted file).
    pub session_file: Option<PathBuf>,
    /// Current leaf entry id after the operation.
    pub leaf_id: Option<String>,
    /// upstream fork `selectedText` (fork-before a user message).
    pub selected_text: Option<String>,
}

/// One live session's native host surface.
///
/// Every call upgrades the `Weak` session: a dropped session reports
/// [`NativeHostError::Unsupported`] instead of panicking. Cloneable.
#[derive(Clone)]
pub struct NativeSessionHost {
    session: Weak<AgentSession>,
}

impl std::fmt::Debug for NativeSessionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSessionHost")
            .field("live", &self.session.strong_count())
            .finish()
    }
}

impl NativeSessionHost {
    /// Build a handle for one live session.
    pub fn for_session(session: &Arc<AgentSession>) -> Self {
        Self {
            session: Arc::downgrade(session),
        }
    }

    fn upgrade(&self) -> Result<Arc<AgentSession>, NativeHostError> {
        self.session
            .upgrade()
            .ok_or_else(|| NativeHostError::unsupported("the session has been dropped"))
    }

    /// The current session id (live read).
    pub fn session_id(&self) -> Option<String> {
        self.session
            .upgrade()
            .and_then(|session| session.session_id())
    }

    /// Whether `action` is available right now.
    pub fn capability(&self, action: NativeSessionAction) -> NativeCapability {
        match self.session.upgrade() {
            Some(session) => session.native_session_capability(action),
            None => NativeCapability::Unsupported {
                reason: "the session has been dropped".to_owned(),
            },
        }
    }

    /// `ctx.newSession` with a typed result and caller cancellation.
    pub fn new_session(
        &self,
        options: NewSessionOptions,
        cancel: AbortSignal,
    ) -> NativeHostFuture<Result<SessionReplacement, NativeHostError>> {
        let session = self.session.clone();
        Box::pin(async move {
            let session = session
                .upgrade()
                .ok_or_else(|| NativeHostError::unsupported("the session has been dropped"))?;
            let parent = options
                .parent_session
                .as_ref()
                .map(|parent| parent.to_string());
            session.native_new_session(parent.as_deref(), &cancel).await
        })
    }

    /// `ctx.fork` with a typed result and caller cancellation.
    ///
    /// `entry_id: None` forks the session at its current position into a new
    /// file (upstream `clone` semantics, storage replaced);
    /// `entry_id: Some(id)` branches at the entry (`before`/`at` position,
    /// tree branched in place).
    pub fn fork(
        &self,
        entry_id: Option<String>,
        options: ForkOptions,
        cancel: AbortSignal,
    ) -> NativeHostFuture<Result<SessionReplacement, NativeHostError>> {
        let session = self.session.clone();
        let position = options
            .position
            .map(|position| position.as_str().to_owned());
        Box::pin(async move {
            let session = session
                .upgrade()
                .ok_or_else(|| NativeHostError::unsupported("the session has been dropped"))?;
            session
                .native_fork(entry_id.as_deref(), position.as_deref(), &cancel)
                .await
        })
    }

    /// `ctx.switchSession` with a typed result and caller cancellation.
    pub fn switch_session(
        &self,
        path: PathBuf,
        cancel: AbortSignal,
    ) -> NativeHostFuture<Result<SessionReplacement, NativeHostError>> {
        let session = self.session.clone();
        Box::pin(async move {
            let session = session
                .upgrade()
                .ok_or_else(|| NativeHostError::unsupported("the session has been dropped"))?;
            session.native_switch_session(&path, &cancel).await
        })
    }

    // -- UI ----------------------------------------------------------------

    /// Whether a native floating custom overlay can be shown.
    pub fn ui_capability(&self) -> NativeCapability {
        let Ok(session) = self.upgrade() else {
            return NativeCapability::Unsupported {
                reason: "the session has been dropped".to_owned(),
            };
        };
        match &session.extensions {
            None => NativeCapability::Unsupported {
                reason: "no extension runtime is attached, so there is no presentation queue"
                    .to_owned(),
            },
            Some(js) if !js.config().has_ui => NativeCapability::Unsupported {
                reason: "the extension runtime was built without a UI sink".to_owned(),
            },
            Some(_) if session.plugins.is_none() => NativeCapability::Unsupported {
                reason: "the session has no plugin host to answer the overlay".to_owned(),
            },
            Some(_) => NativeCapability::Available,
        }
    }

    /// Show one native custom component as a floating overlay (upstream
    /// `ctx.ui.custom(factory, { overlay: true, overlayOptions })`).
    ///
    /// The request rides the ONE `DialogRequest` queue; `overlayOptions` is
    /// serialized to the exact wire object the TUI already parses. `handle`
    /// is bound to the allocated dialog id, so `hide`/`setHidden` work and a
    /// later TUI `publish_native_overlay_bounds(id, bounds)` fills
    /// `handle.get_bounds()`.
    pub fn custom_overlay(
        &self,
        factory: Arc<dyn OverlayComponentFactory>,
        options: NativeOverlayOptions,
        handle: NativeOverlayHandle,
        timeout: Option<std::time::Duration>,
    ) -> NativeHostFuture<Result<Option<JsonValue>, NativeHostError>> {
        let session = match self.upgrade() {
            Ok(session) => session,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let (Some(js), Some(_)) = (session.extensions.clone(), session.plugins.clone()) else {
            return Box::pin(async move {
                Err(NativeHostError::unsupported(
                    "the session has no extension runtime/plugin host, so no overlay can be shown",
                ))
            });
        };
        if !js.config().has_ui {
            return Box::pin(async move {
                Err(NativeHostError::unsupported(
                    "the extension runtime was built without a UI sink",
                ))
            });
        }
        let request = super::build::plugin_services::native_custom_overlay_request(
            &session, factory, options, handle, timeout,
        );
        Box::pin(async move { request.await.map_err(map_service_error) })
    }
}

impl AgentSession {
    /// This session's native host handle, when it was built through an `Arc`
    /// constructor (only those can hand out a `Weak`).
    pub fn native_host(&self) -> Option<NativeSessionHost> {
        let session = self
            .self_weak
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)?;
        Some(NativeSessionHost::for_session(&session))
    }
}

/// A native custom-overlay factory that carries floating options into the
/// EXISTING `UiService::custom` method.
///
/// The service trait's signature cannot take an option bag, so this wrapper
/// registers its [`NativeOverlayOptions`] and [`NativeOverlayHandle`] while
/// the service calls `create`; the service consumes the spec immediately
/// after, serializes `overlay: true` + `overlayOptions`, and binds the
/// handle. Use it as the factory argument:
///
/// ```ignore
/// let factory = NativeOverlayFactory::new(inner, options, handle);
/// ui.custom(factory, DialogOptions::default()).await
/// ```
///
/// Without this wrapper, `UiService::custom` keeps the documented editor-slot
/// default; the result-returning, explicitly-typed path is
/// [`NativeSessionHost::custom_overlay`].
pub struct NativeOverlayFactory {
    inner: Arc<dyn OverlayComponentFactory>,
    options: NativeOverlayOptions,
    handle: NativeOverlayHandle,
}

impl NativeOverlayFactory {
    /// Wrap `inner` with one floating option bag; returns the service-facing
    /// factory object.
    // `new` intentionally returns the service-facing trait object rather than
    // `Self`; that is the value the `UiService::custom` call expects.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        inner: Arc<dyn OverlayComponentFactory>,
        options: NativeOverlayOptions,
        handle: NativeOverlayHandle,
    ) -> Arc<dyn OverlayComponentFactory> {
        Arc::new(Self {
            inner,
            options,
            handle,
        })
    }
}

impl OverlayComponentFactory for NativeOverlayFactory {
    fn create(&self, done: OverlayDone) -> Option<Arc<dyn OverlayComponent>> {
        // Register for the concrete allocation address: the service holds
        // this exact allocation as `Arc<dyn OverlayComponentFactory>`, and
        // unsizing keeps the data pointer. The service consumes the spec
        // right after `create` returns, so a concurrent custom call on the
        // SAME wrapper pops an equivalent spec (identical options/handle).
        crate::extensions::overlay::register_overlay_factory_spec(
            self as *const Self as usize,
            OverlayFactorySpec {
                options: self.options.clone(),
                handle: self.handle.clone(),
            },
        );
        self.inner.create(done)
    }
}

// ---------------------------------------------------------------------------
// Process-wide registry (session id -> live session)
// ---------------------------------------------------------------------------

static NATIVE_HOSTS: Mutex<BTreeMap<String, Weak<AgentSession>>> = Mutex::new(BTreeMap::new());

/// Register (or re-key) the native host entry for a live `Arc` session.
///
/// Called by the `Arc` constructors and again after every storage swap
/// (`AgentSession::swap_session_storage`), so `newSession` / `fork` /
/// `switchSession` keep the id→session mapping current. Dead entries are
/// pruned on every register and lookup.
pub(crate) fn register_native_session_host(session: &Arc<AgentSession>) {
    let Some(id) = session.session_id() else {
        return;
    };
    let mut hosts = NATIVE_HOSTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hosts.retain(|key, weak| {
        let Some(live) = weak.upgrade() else {
            return false;
        };
        if Arc::ptr_eq(&live, session) {
            // This session moved ids: drop every stale key it owned.
            return key == &id;
        }
        live.session_id().is_some()
    });
    hosts.insert(id, Arc::downgrade(session));
}

fn live_session_for_id(id: &str) -> Option<Arc<AgentSession>> {
    let weak = {
        let hosts = NATIVE_HOSTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hosts.get(id).cloned()?
    };
    let session = weak.upgrade()?;
    if session.session_id().as_deref() != Some(id) {
        // Replacement already re-keyed this session; the old key is stale.
        NATIVE_HOSTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        return None;
    }
    Some(session)
}

/// Resolve the native host for the live session behind a plugin context.
///
/// This is the native-plugin entry point: it needs no new service method, so
/// the existing `PluginContext`/`SessionService` ABI is untouched. A stale
/// context, an ephemeral session, or a session built by value reports
/// [`NativeHostError::Unsupported`] with the exact reason.
pub fn native_session_host(ctx: &PluginContext) -> Result<NativeSessionHost, NativeHostError> {
    if !ctx.is_active() {
        return Err(NativeHostError::unsupported(format!(
            "stale plugin context (bound at epoch {}, host is at epoch {})",
            ctx.epoch(),
            ctx.live_epoch()
        )));
    }
    let session = ctx
        .session()
        .ok_or_else(|| NativeHostError::unsupported("no session service is bound"))?;
    let id = session.session_id().ok_or_else(|| {
        NativeHostError::unsupported(
            "the session has no id (ephemeral or detached); no native host is registered",
        )
    })?;
    let session = live_session_for_id(&id).ok_or_else(|| {
        NativeHostError::unsupported(format!(
            "no native session host is registered for session `{id}`; only an Arc-constructed \
             session with attached storage registers one"
        ))
    })?;
    Ok(NativeSessionHost::for_session(&session))
}

fn map_service_error(error: PluginServiceError) -> NativeHostError {
    match error {
        PluginServiceError::Unavailable { .. } | PluginServiceError::Stale { .. } => {
            NativeHostError::unsupported(error.to_string())
        }
        PluginServiceError::Failed(message) => NativeHostError::Failed { message },
    }
}
