use super::*;

/// Minimum interval between two `provider:refreshModels` dispatches for the
/// same provider definition. The hook performs a network refresh (and a model
/// store write), and `sync_plugin_providers` runs on every prompt; without
/// this gate every prompt paid one refresh round trip even though the model
/// list changes on the order of hours. A changed definition (different JSON
/// fingerprint) bypasses the window.
const REFRESH_MODELS_WINDOW: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Last refresh of one provider id: the definition fingerprint it was
/// refreshed for, when that happened, and the model list the refresh returned
/// (the wire `models` array, `None` when the refresh produced no usable list).
///
/// The cache is scoped to one session's `ProviderRegistry` (its pointer is the
/// map key): every session has its own registry and its own JS refresh
/// callbacks, so a second session must dispatch again and receive that
/// session's `context.stored`, even when the provider id and definition are
/// identical. Sharing the stamp process-wide leaked one session's refresh into
/// a concurrent session (and skipped the callback entirely).
struct RefreshStamp {
    fingerprint: u64,
    last_refresh: std::time::Instant,
    models: Option<JsonValue>,
}

fn refresh_stamps() -> &'static std::sync::Mutex<std::collections::HashMap<String, RefreshStamp>> {
    static STAMPS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, RefreshStamp>>,
    > = std::sync::OnceLock::new();
    STAMPS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn refresh_stamp_key(scope: usize, id: &str) -> String {
    format!("{scope}:{id}")
}

/// Fingerprint of one provider definition: any change to the definition (new
/// models, baseUrl, auth, a replaced callback marker) must re-run the refresh.
fn provider_config_fingerprint(config: &JsonValue) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let text = serde_json::to_string(config).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Whether `id`'s `refreshModels` hook should run now: first sight, a changed
/// definition, or a window older than [`REFRESH_MODELS_WINDOW`].
fn refresh_models_due(scope: usize, id: &str, fingerprint: u64) -> bool {
    let now = std::time::Instant::now();
    let stamps = refresh_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match stamps.get(&refresh_stamp_key(scope, id)) {
        Some(stamp) => {
            stamp.fingerprint != fingerprint
                || now.duration_since(stamp.last_refresh) >= REFRESH_MODELS_WINDOW
        }
        None => true,
    }
}

/// Last successful provider-definition sync for one `(ProviderRegistry,
/// AgentSession)` pair: the resolved definition-set fingerprint, the model
/// content each definition installed, and the exact provider allocations this
/// session left in the dynamic map. When all still match,
/// [`AgentSession::sync_plugin_providers`] returns without rebuilding the
/// dynamic provider `Arc`s (P2-新-1): re-registering an unchanged definition
/// used to replace every provider allocation on every prompt, which in turn
/// invalidated the session catalog fingerprint by address.
///
/// The key carries the SESSION identity in addition to
/// `ProviderRegistry::refresh_scope_id()`: a subagent child shares the
/// parent's `Arc<ProviderRegistry>` (and therefore its scope id) but installs
/// its own `extension_api_keys` and OAuth wrappers in `sync_plugin_providers`.
/// A registry-only stamp let the child's first sync short-circuit on the
/// parent's stamp, silently leaving the child without its plugin API keys.
struct ProviderSyncStamp {
    /// Weak registry identity: a dead registry's stamp is pruned on the next
    /// insert instead of accumulating for every registry in the process.
    registry: std::sync::Weak<ProviderRegistry>,
    /// Weak session identity (the second half of the map key). Kept separate
    /// from the registry weak: a child session dies while the parent's shared
    /// registry (and its other stamps) stays alive, and must still be pruned.
    session: std::sync::Weak<std::sync::atomic::AtomicU8>,
    definitions: u64,
    /// `(provider id, installed model list hash)` in definition order. A
    /// runtime registration that replaced or removed a provider between two
    /// syncs changes this, so the fast path cannot hide an external update.
    models: Vec<(String, u64)>,
    /// Whether this stamp came from a fully-applied, non-live pass and may be
    /// used to skip the next sync. Cleanup-only stamps (written by a live or
    /// partially-applied pass solely to record `installed` for child
    /// teardown) are always `false`, so they can never short-circuit.
    fast_path: bool,
    /// The exact dynamic provider allocation this session installed for each
    /// definition, as `Weak`s, plus the entry the installation displaced. A
    /// shared `ProviderRegistry` lets a subagent child overwrite the parent's
    /// entries (and vice versa); comparing only definitions/models would keep
    /// the parent's fast path green while its `OAuthFlowOverlay` still points
    /// at the child's extension handle. The `Weak` does not extend provider
    /// lifetimes (SNAP4 P2-新-1).
    installed: Vec<(String, ProviderInstallation)>,
}

/// One dynamic provider a session installed, plus the chain of entries the
/// installation displaced (nearest first).
///
/// `previous` is a STRONG reference chain on purpose: the parent provider is
/// no longer in the shared map once the child overwrote it, so only this
/// record keeps the exact allocation alive for the child-teardown restore
/// (SNAP6 P1-1). The provider itself only holds weak runtime bindings
/// (SNAP4), so the extra strong reference cannot pin the parent's extension
/// runtime.
///
/// Each node records the session that owned it (when its installation could
/// be found in a live stamp). Teardown walks the chain and skips nodes whose
/// owner has torn down, so two parallel children that finish out of order
/// restore the live parent instead of re-installing the already-finished
/// sibling's wrapper (SNAP7 P2-3).
#[derive(Clone)]
struct ProviderInstallation {
    installed: std::sync::Weak<dyn crate::ai::provider::Provider>,
    previous: Vec<PreviousProvider>,
}

/// One displaced dynamic entry in a [`ProviderInstallation`] restore chain.
#[derive(Clone)]
pub(crate) struct PreviousProvider {
    provider: crate::ai::provider::ProviderBox,
    /// The installation that owned `provider`, when known. `None` means the
    /// entry did not belong to any tracked session or live native binding when
    /// it was displaced (e.g. an external/static-overlay registration), so it
    /// has no teardown that could invalidate it.
    owner: Option<PreviousInstallationOwner>,
}

/// Who installed one displaced entry (SNAP7's owner/liveness stamp, extended
/// with the native `ProviderService` binding token the PROVIDER3 lane needs
/// for runtime JSON/JSON-backed virtual registrations).
#[derive(Clone)]
enum PreviousInstallationOwner {
    /// A session's provider-sync pass.
    Session {
        key: ProviderSyncStampKey,
        session: std::sync::Weak<std::sync::atomic::AtomicU8>,
    },
    /// A live [`crate::plugins::services::ProviderService`] binding, which
    /// records its own dynamic installs in [`native_dynamic_installations`].
    Binding {
        scope: u64,
        binding: std::sync::Weak<std::sync::atomic::AtomicU8>,
    },
}

/// One live native runtime-JSON provider installation: the exact allocation
/// this binding owns, its liveness token, and the restore chain it displaced.
/// Mirrors the session stamp's `installed` record so a parent/sibling native
/// service can be recognized as the owner of an entry it installed (SNAP7
/// P2-S7-2: a native registration used to look like `owner=None`, i.e.
/// permanently live).
struct NativeDynamicInstallation {
    installed: std::sync::Weak<dyn crate::ai::provider::Provider>,
    owner: std::sync::Weak<std::sync::atomic::AtomicU8>,
    previous: Vec<PreviousProvider>,
}

type NativeDynamicKey = (u64, String, usize);
type NativeDynamicInstallations =
    std::sync::Mutex<std::collections::HashMap<NativeDynamicKey, NativeDynamicInstallation>>;

fn native_dynamic_installations() -> &'static NativeDynamicInstallations {
    static INSTALLATIONS: std::sync::OnceLock<NativeDynamicInstallations> =
        std::sync::OnceLock::new();
    INSTALLATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn retain_live_native_dynamic_installations(
    installations: &mut std::collections::HashMap<NativeDynamicKey, NativeDynamicInstallation>,
) {
    installations.retain(|_, installation| installation.owner.strong_count() > 0);
}

/// One virtual-model installation installed by a native binding. The route
/// callback is part of the entry: `VirtualModelRegistry::register` does not
/// touch callbacks, so restoring only the registration would leave a dead
/// owner's callback routing live requests.
#[derive(Clone)]
pub(crate) struct PreviousVirtualModel {
    pub(crate) registration: crate::ai::virtual_::VirtualModelRegistration,
    pub(crate) route: Option<crate::ai::virtual_::VirtualRouteFn>,
    owner: Option<PreviousInstallationOwner>,
}

struct NativeVirtualModelInstallation {
    owner: std::sync::Weak<std::sync::atomic::AtomicU8>,
    registration: crate::ai::virtual_::VirtualModelRegistration,
    previous: Vec<PreviousVirtualModel>,
}

type NativeVirtualModelKey = (u64, String, String, usize);
type NativeVirtualModelInstallations = std::sync::Mutex<
    std::collections::HashMap<NativeVirtualModelKey, NativeVirtualModelInstallation>,
>;

fn native_virtual_model_installations() -> &'static NativeVirtualModelInstallations {
    static INSTALLATIONS: std::sync::OnceLock<NativeVirtualModelInstallations> =
        std::sync::OnceLock::new();
    INSTALLATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn retain_live_native_virtual_model_installations(
    installations: &mut std::collections::HashMap<
        NativeVirtualModelKey,
        NativeVirtualModelInstallation,
    >,
) {
    installations.retain(|_, installation| installation.owner.strong_count() > 0);
}

type ProviderSyncStampKey = (u64, usize);
type ProviderSyncStamps =
    std::sync::Mutex<std::collections::HashMap<ProviderSyncStampKey, ProviderSyncStamp>>;

fn provider_sync_stamps() -> &'static ProviderSyncStamps {
    static STAMPS: std::sync::OnceLock<ProviderSyncStamps> = std::sync::OnceLock::new();
    STAMPS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Process-unique identity of THIS session (never of its provider registry).
/// `session_state` is a per-`AgentSession` `Arc` created by the build and
/// never replaced; unlike `session_id` it exists for ephemeral sessions
/// (including the subagent child) too.
fn provider_sync_session_identity(session: &AgentSession) -> usize {
    std::sync::Arc::as_ptr(&session.session_state) as usize
}

fn provider_sync_session_handle(
    session: &AgentSession,
) -> std::sync::Weak<std::sync::atomic::AtomicU8> {
    std::sync::Arc::downgrade(&session.session_state)
}

impl AgentSession {
    /// This session's provider-resolution identity inside its registry
    /// (SNAP8 P2-1). Shared by provider sync, `/login`, credential refresh and
    /// request dispatch so all four consult the same per-session overlay.
    pub(crate) fn provider_session_key(&self) -> crate::ai::provider::ProviderSessionKey {
        crate::ai::provider::ProviderSessionKey::new(
            self.registry.refresh_scope_id(),
            &self.session_state,
        )
    }

    /// Test seam: the key a test passes to the session-scoped registry
    /// lookups/`/login`/refresh entry points.
    #[doc(hidden)]
    pub fn provider_session_key_for_test(&self) -> crate::ai::provider::ProviderSessionKey {
        self.provider_session_key()
    }

    /// Session-scoped completion callback for compaction runs driven from the
    /// prompt loop. The legacy callback calls
    /// `ProviderRegistry::stream_resolving_auth` without a session, so while a
    /// subagent child is alive the parent's threshold/overflow compaction can
    /// resolve the child's provider (and API key/runtime). This variant keeps
    /// the same request options but resolves the provider and credential
    /// through THIS session's overlay (SNAP9 P2-S8-4).
    pub(super) fn complete_fn_session_scoped(&self) -> Arc<crate::compaction::CompleteFn> {
        let signal = self
            .signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.complete_fn_for_signal_session_scoped(&self.current_model(), signal)
    }

    /// [`Self::complete_fn_session_scoped`] against an explicit model (the
    /// routed model of a virtual selection). `signal` is the compaction or
    /// branch-summary abort signal, carried on the same request options the
    /// legacy path uses.
    pub(super) fn complete_fn_for_signal_session_scoped(
        &self,
        model: &Model,
        signal: AbortSignal,
    ) -> Arc<crate::compaction::CompleteFn> {
        let options = self.stream_options_with_actor_hooks(model, Some(signal), true);
        self.complete_closure_session_scoped(
            model,
            options,
            "summarization stream did not terminate",
        )
    }

    /// Session-scoped `ctx.modelRegistry.complete` host op. Same
    /// `actor_hooks: false` contract as
    /// [`Self::stream_options_with_actor_hooks`](crate::agent_session::stream):
    /// the call runs inside the extension actor, so inline provider hooks
    /// would re-enter it.
    pub(super) fn complete_fn_for_host_op_session_scoped(
        &self,
        model: &Model,
    ) -> Arc<crate::compaction::CompleteFn> {
        let options = self.stream_options_with_actor_hooks(model, None, false);
        self.complete_closure_session_scoped(model, options, "completion stream did not terminate")
    }

    /// Shared tail of the session-scoped completion callbacks: stream through
    /// `stream_resolving_auth_for(Some(session))` so provider/credential
    /// resolution never falls back to the process-shared last writer.
    fn complete_closure_session_scoped(
        &self,
        model: &Model,
        options: StreamOptions,
        missing_message: &'static str,
    ) -> Arc<crate::compaction::CompleteFn> {
        let registry = Arc::clone(&self.registry);
        let session_key = self.provider_session_key();
        let model = model.clone();
        Arc::new(Box::new(move |context| {
            let registry = Arc::clone(&registry);
            let session_key = session_key.clone();
            let model = model.clone();
            let options = options.clone();
            Box::pin(async move {
                let stream =
                    registry.stream_resolving_auth_for(Some(&session_key), model, context, options);
                // Completion consumer: drain while waiting. With the bounded
                // event queue, a consumer that never pops an event parks the
                // provider producer forever (`drain_and_result` discards the
                // events this caller will never read).
                match stream.drain_and_result().await {
                    Some(message) => Ok((*message).clone()),
                    None => Err(missing_message.to_owned()),
                }
            })
        }))
    }
}

/// Registry-backed stream callback scoped to one session (SNAP8 P2-1).
///
/// Mirrors [`crate::agent_session::events::registry_event_stream`] but
/// resolves the provider (and any OAuth credential refresh) through
/// `session_key`'s overlay first, so a subagent child sharing the registry
/// cannot redirect the parent's in-flight model request to its own extension
/// runtime.
pub(super) fn session_registry_event_stream(
    registry: Arc<ProviderRegistry>,
    session_key: crate::ai::provider::ProviderSessionKey,
    model: crate::ai::types::Model,
    options: crate::ai::types::StreamOptions,
) -> EventStreamFn {
    let base_thinking = options
        .reasoning
        .unwrap_or(crate::ai::types::ThinkingLevel::Off);
    Box::new(move |request: ProviderRequest| {
        let model = request.model.unwrap_or_else(|| model.clone());
        let mut options = options.clone();
        if request.thinking_level != base_thinking {
            options.reasoning = Some(request.thinking_level);
        }
        let context = crate::agent::agent_loop::context_into_owned(request.context);
        let stream =
            registry.stream_resolving_auth_for(Some(&session_key), model, context, options);
        Box::pin(async move { stream })
    })
}

fn retain_live_provider_sync_stamps(
    stamps: &mut std::collections::HashMap<ProviderSyncStampKey, ProviderSyncStamp>,
) {
    stamps.retain(|_, stamp| stamp.registry.strong_count() > 0 && stamp.session.strong_count() > 0);
}

/// Find the live installation that currently claims `provider` for `id` in
/// this registry scope: its owner (session stamp or native binding token) and
/// the restore chain that installation recorded.
fn provider_installation_owner(
    scope: u64,
    id: &str,
    provider: &crate::ai::provider::ProviderBox,
) -> Option<(PreviousInstallationOwner, Vec<PreviousProvider>)> {
    let session_owner = {
        let mut stamps = provider_sync_stamps()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retain_live_provider_sync_stamps(&mut stamps);
        stamps
            .iter()
            .filter(|((stamp_scope, _), _)| *stamp_scope == scope)
            .find_map(|(key, stamp)| {
                stamp
                    .installed
                    .iter()
                    .find_map(|(installed_id, installation)| {
                        (installed_id == id
                            && installation.installed.upgrade().is_some_and(|installed| {
                                std::sync::Arc::ptr_eq(&installed, provider)
                            }))
                        .then(|| {
                            (
                                PreviousInstallationOwner::Session {
                                    key: *key,
                                    session: stamp.session.clone(),
                                },
                                installation.previous.clone(),
                            )
                        })
                    })
            })
    };
    if session_owner.is_some() {
        return session_owner;
    }
    native_dynamic_installation_owner(scope, id, provider)
}

/// The native `ProviderService` installation that claims `provider` for `id`.
fn native_dynamic_installation_owner(
    scope: u64,
    id: &str,
    provider: &crate::ai::provider::ProviderBox,
) -> Option<(PreviousInstallationOwner, Vec<PreviousProvider>)> {
    let mut installations = native_dynamic_installations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_live_native_dynamic_installations(&mut installations);
    installations.iter().find_map(
        |((installed_scope, installed_id, binding_ptr), installation)| {
            if *installed_scope != scope || installed_id != id {
                return None;
            }
            let installed = installation.installed.upgrade()?;
            if !std::sync::Arc::ptr_eq(&installed, provider) {
                return None;
            }
            let binding = installation.owner.upgrade()?;
            if std::sync::Arc::as_ptr(&binding) as usize != *binding_ptr {
                return None;
            }
            Some((
                PreviousInstallationOwner::Binding {
                    scope,
                    binding: installation.owner.clone(),
                },
                installation.previous.clone(),
            ))
        },
    )
}

/// Build the restore chain for one displaced dynamic entry: the entry itself
/// when a live session stamp or native binding claims it, followed by that
/// installation's own chain. Entries installed by the current session are
/// dropped (a session must never restore its own stale wrapper); the older
/// entries they displaced are kept.
fn provider_previous_chain(
    scope: u64,
    session: usize,
    id: &str,
    displaced: &crate::ai::provider::ProviderBox,
) -> Vec<PreviousProvider> {
    let mut chain = Vec::new();
    match provider_installation_owner(scope, id, displaced) {
        Some((owner, nested)) => {
            chain.push(PreviousProvider {
                provider: displaced.clone(),
                owner: Some(owner),
            });
            chain.extend(nested);
        }
        None => chain.push(PreviousProvider {
            provider: displaced.clone(),
            owner: None,
        }),
    }
    chain.retain(|entry| {
        entry
            .owner
            .as_ref()
            .is_none_or(|owner| !owner.belongs_to_session(session))
    });
    dedup_provider_chain(chain)
}

fn dedup_provider_chain(chain: Vec<PreviousProvider>) -> Vec<PreviousProvider> {
    let mut deduped: Vec<PreviousProvider> = Vec::with_capacity(chain.len());
    for entry in chain {
        if deduped
            .iter()
            .any(|existing| std::sync::Arc::ptr_eq(&existing.provider, &entry.provider))
        {
            continue;
        }
        deduped.push(entry);
    }
    deduped
}

/// Build the restore chain for one entry displaced by a NATIVE binding's
/// runtime registration. Re-registering the same id from the same binding
/// keeps the chain recorded by its FIRST installation, so teardown can never
/// unwind into a dead intermediate the binding itself installed.
pub(crate) fn native_dynamic_previous_chain(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    id: &str,
    displaced: Option<&crate::ai::provider::ProviderBox>,
) -> Vec<PreviousProvider> {
    let Some(displaced) = displaced else {
        return Vec::new();
    };
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    {
        let mut installations = native_dynamic_installations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retain_live_native_dynamic_installations(&mut installations);
        if let Some(installation) = installations.get(&(scope, id.to_owned(), binding_ptr)) {
            if installation
                .installed
                .upgrade()
                .is_some_and(|installed| std::sync::Arc::ptr_eq(&installed, displaced))
            {
                return installation.previous.clone();
            }
        }
    }
    let mut chain = Vec::new();
    match provider_installation_owner(scope, id, displaced) {
        Some((owner, nested)) => {
            chain.push(PreviousProvider {
                provider: displaced.clone(),
                owner: Some(owner),
            });
            chain.extend(nested);
        }
        None => chain.push(PreviousProvider {
            provider: displaced.clone(),
            owner: None,
        }),
    }
    chain.retain(|entry| {
        entry
            .owner
            .as_ref()
            .is_none_or(|owner| !owner.belongs_to_binding(scope, binding_ptr))
    });
    dedup_provider_chain(chain)
}

/// Record (or replace) one native binding's runtime-JSON installation. The
/// chain doubles as the cross-binding ownership ledger consulted by another
/// native service's [`native_dynamic_previous_chain`].
pub(crate) fn record_native_dynamic_installation(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    id: &str,
    installed: std::sync::Weak<dyn crate::ai::provider::Provider>,
    previous: Vec<PreviousProvider>,
) {
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    let mut installations = native_dynamic_installations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_live_native_dynamic_installations(&mut installations);
    installations.insert(
        (scope, id.to_owned(), binding_ptr),
        NativeDynamicInstallation {
            installed,
            owner: std::sync::Arc::downgrade(binding),
            previous,
        },
    );
}

/// Drop one native binding's ownership record (teardown decided the dynamic
/// entry; the restore itself is identity-checked by the registry).
pub(crate) fn forget_native_dynamic_installation(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    id: &str,
) {
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    let mut installations = native_dynamic_installations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    installations.remove(&(scope, id.to_owned(), binding_ptr));
    retain_live_native_dynamic_installations(&mut installations);
}

/// Remove every JSON/virtual ownership record of one binding (defensive Drop
/// sweep; the per-entry restore paths already remove their own records).
pub(crate) fn forget_native_binding_installations(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
) {
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    {
        let mut installations = native_dynamic_installations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        installations.retain(|(record_scope, _, record_binding), _| {
            *record_scope != scope || *record_binding != binding_ptr
        });
        retain_live_native_dynamic_installations(&mut installations);
    }
    {
        let mut installations = native_virtual_model_installations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        installations.retain(|(record_scope, _, _, record_binding), _| {
            *record_scope != scope || *record_binding != binding_ptr
        });
        retain_live_native_virtual_model_installations(&mut installations);
    }
}

impl PreviousInstallationOwner {
    /// Whether this chain node may still be installed: its session/binding is
    /// alive AND its installation record still claims this exact allocation.
    /// A sibling that finished first removed its record (and its liveness
    /// token dies with the child), so the restore walk continues to the next
    /// parent entry instead of resurrecting the finished sibling.
    fn is_live(&self, id: &str, provider: &crate::ai::provider::ProviderBox) -> bool {
        match self {
            Self::Session { key, session } => {
                if session.strong_count() == 0 {
                    return false;
                }
                let mut stamps = provider_sync_stamps()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                retain_live_provider_sync_stamps(&mut stamps);
                stamps.get(key).is_some_and(|stamp| {
                    stamp.installed.iter().any(|(installed_id, installation)| {
                        installed_id == id
                            && installation.installed.upgrade().is_some_and(|installed| {
                                std::sync::Arc::ptr_eq(&installed, provider)
                            })
                    })
                })
            }
            Self::Binding { scope, binding } => {
                if binding.strong_count() == 0 {
                    return false;
                }
                native_dynamic_installation_owner(*scope, id, provider)
                    .is_some_and(|(owner, _)| owner.is_same_binding(*scope, binding))
            }
        }
    }

    fn belongs_to_session(&self, session: usize) -> bool {
        matches!(self, Self::Session { key, .. } if key.1 == session)
    }

    fn belongs_to_binding(&self, scope: u64, binding: usize) -> bool {
        matches!(
            self,
            Self::Binding {
                scope: owner_scope,
                binding: owner_binding,
            } if *owner_scope == scope
                && owner_binding.strong_count() > 0
                && owner_binding.as_ptr() as usize == binding
        )
    }

    fn is_same_binding(
        &self,
        scope: u64,
        binding: &std::sync::Weak<std::sync::atomic::AtomicU8>,
    ) -> bool {
        match self {
            Self::Binding {
                scope: owner_scope,
                binding: owner_binding,
            } => *owner_scope == scope && owner_binding.ptr_eq(binding),
            Self::Session { .. } => false,
        }
    }

    /// Virtual registrations have no provider allocation; liveness re-checks
    /// the owner's recorded registration (and route callback identity) so a
    /// re-registration by the same binding invalidates its older chain node.
    fn is_live_virtual(
        &self,
        provider: &str,
        id: &str,
        registration: &crate::ai::virtual_::VirtualModelRegistration,
        route: &Option<crate::ai::virtual_::VirtualRouteFn>,
    ) -> bool {
        match self {
            Self::Session { session, .. } => {
                // Session-bound virtual registrations are tracked only when a
                // native binding installs them; a bare session owner means
                // the session still exists.
                session.strong_count() > 0
            }
            Self::Binding { scope, binding } => {
                if binding.strong_count() == 0 {
                    return false;
                }
                native_virtual_model_installation_owner(*scope, provider, id, registration, route)
                    .is_some_and(|(owner, _)| owner.is_same_binding(*scope, binding))
            }
        }
    }
}

fn native_virtual_model_installation_owner(
    scope: u64,
    provider: &str,
    id: &str,
    registration: &crate::ai::virtual_::VirtualModelRegistration,
    route: &Option<crate::ai::virtual_::VirtualRouteFn>,
) -> Option<(PreviousInstallationOwner, Vec<PreviousVirtualModel>)> {
    let mut installations = native_virtual_model_installations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_live_native_virtual_model_installations(&mut installations);
    installations.iter().find_map(
        |((installed_scope, installed_provider, installed_id, binding_ptr), installation)| {
            if *installed_scope != scope
                || installed_provider != provider
                || installed_id != id
                || &installation.registration != registration
            {
                return None;
            }
            let binding = installation.owner.upgrade()?;
            if std::sync::Arc::as_ptr(&binding) as usize != *binding_ptr {
                return None;
            }
            // The registration carries this binding's `native-plugin:<id>`
            // source, so it identifies the installer even when a caller
            // installed/replaced the route callback out-of-band.
            let _ = route;
            Some((
                PreviousInstallationOwner::Binding {
                    scope,
                    binding: installation.owner.clone(),
                },
                installation.previous.clone(),
            ))
        },
    )
}

fn same_virtual_route(
    left: &Option<crate::ai::virtual_::VirtualRouteFn>,
    right: &Option<crate::ai::virtual_::VirtualRouteFn>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
        _ => false,
    }
}

/// Build the restore chain for one virtual-model entry displaced by a NATIVE
/// binding. Re-registering the same key from the same binding keeps the
/// original chain (the ledger record's registration matches the displaced
/// value).
pub(crate) fn native_virtual_previous_chain(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    provider: &str,
    id: &str,
    displaced: Option<&crate::ai::virtual_::VirtualModelRegistration>,
    displaced_route: Option<&crate::ai::virtual_::VirtualRouteFn>,
) -> Vec<PreviousVirtualModel> {
    let Some(displaced) = displaced else {
        return Vec::new();
    };
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    let route = displaced_route.cloned();
    {
        let mut installations = native_virtual_model_installations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retain_live_native_virtual_model_installations(&mut installations);
        if let Some(installation) =
            installations.get(&(scope, provider.to_owned(), id.to_owned(), binding_ptr))
        {
            if &installation.registration == displaced {
                return installation.previous.clone();
            }
        }
    }
    let mut chain = Vec::new();
    match native_virtual_model_installation_owner(scope, provider, id, displaced, &route) {
        Some((owner, nested)) => {
            chain.push(PreviousVirtualModel {
                registration: displaced.clone(),
                route,
                owner: Some(owner),
            });
            chain.extend(nested);
        }
        None => chain.push(PreviousVirtualModel {
            registration: displaced.clone(),
            route,
            owner: None,
        }),
    }
    chain.retain(|entry| {
        entry
            .owner
            .as_ref()
            .is_none_or(|owner| !owner.belongs_to_binding(scope, binding_ptr))
    });
    dedup_virtual_chain(chain)
}

fn dedup_virtual_chain(chain: Vec<PreviousVirtualModel>) -> Vec<PreviousVirtualModel> {
    let mut deduped: Vec<PreviousVirtualModel> = Vec::with_capacity(chain.len());
    for entry in chain {
        if deduped.iter().any(|existing| {
            existing.registration == entry.registration
                && same_virtual_route(&existing.route, &entry.route)
        }) {
            continue;
        }
        deduped.push(entry);
    }
    deduped
}

pub(crate) fn record_native_virtual_installation(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    provider: &str,
    id: &str,
    registration: crate::ai::virtual_::VirtualModelRegistration,
    previous: Vec<PreviousVirtualModel>,
) {
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    let mut installations = native_virtual_model_installations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_live_native_virtual_model_installations(&mut installations);
    installations.insert(
        (scope, provider.to_owned(), id.to_owned(), binding_ptr),
        NativeVirtualModelInstallation {
            owner: std::sync::Arc::downgrade(binding),
            registration,
            previous,
        },
    );
}

pub(crate) fn forget_native_virtual_installation(
    registry: &ProviderRegistry,
    binding: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    provider: &str,
    id: &str,
) {
    let scope = registry.refresh_scope_id();
    let binding_ptr = std::sync::Arc::as_ptr(binding) as usize;
    let mut installations = native_virtual_model_installations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    installations.remove(&(scope, provider.to_owned(), id.to_owned(), binding_ptr));
    retain_live_native_virtual_model_installations(&mut installations);
}

/// The nearest still-live entry in a virtual restore chain, including its
/// route callback. `None` means every displaced owner has torn down, so
/// teardown removes the key (and its callback).
pub(crate) fn resolve_previous_virtual_model(
    previous: &[PreviousVirtualModel],
) -> Option<(
    crate::ai::virtual_::VirtualModelRegistration,
    Option<crate::ai::virtual_::VirtualRouteFn>,
)> {
    previous
        .iter()
        .find(|entry| match &entry.owner {
            None => true,
            Some(owner) => owner.is_live_virtual(
                &entry.registration.provider,
                &entry.registration.id,
                &entry.registration,
                &entry.route,
            ),
        })
        .map(|entry| (entry.registration.clone(), entry.route.clone()))
}

/// The nearest still-live entry in a restore chain. `None` means every
/// displaced owner has torn down, so teardown removes the dynamic entry.
pub(crate) fn resolve_previous_provider(
    id: &str,
    previous: &[PreviousProvider],
) -> Option<crate::ai::provider::ProviderBox> {
    previous
        .iter()
        .find(|entry| match &entry.owner {
            None => true,
            Some(owner) => owner.is_live(id, &entry.provider),
        })
        .map(|entry| entry.provider.clone())
}

/// Record the provider allocation THIS pass installed for `id`. Recorded at
/// registration time instead of re-reading the shared dynamic map at the end
/// of the pass: a concurrent sibling sync can replace the entry between the
/// registration and a post-pass snapshot, which would otherwise stamp the
/// sibling's wrapper as "ours" and defeat the identity check (SNAP4 P2-新-1).
/// A duplicate definition id keeps the last installation, matching the
/// dynamic map's last-writer-wins registration.
fn record_provider_installation(
    registry: &ProviderRegistry,
    session: &crate::ai::provider::ProviderSessionKey,
    installed: &mut Vec<(String, std::sync::Weak<dyn crate::ai::provider::Provider>)>,
    id: &str,
    provider: &crate::ai::provider::ProviderBox,
) {
    // Session overlay first: the model request, `/login` and credential
    // refresh paths resolve this allocation for THIS session even if a
    // sibling/child later overwrites the process-shared dynamic map.
    registry.record_session_sync_dynamic_provider(session, id, provider);
    let entry = (id.to_owned(), std::sync::Arc::downgrade(provider));
    match installed.iter_mut().find(|(existing, _)| existing == id) {
        Some(slot) => *slot = entry,
        None => installed.push(entry),
    }
}

/// Whether every provider this session installed is still the exact
/// allocation the shared dynamic map resolves to. A child session sharing the
/// parent's registry replaces these entries on its own sync; the mismatch
/// forces the parent (and any sibling child) to re-install its own OAuth
/// wrapper/keys instead of short-circuiting into another session's state.
fn provider_sync_installations_match(
    registry: &ProviderRegistry,
    installed: &[(String, ProviderInstallation)],
) -> bool {
    let dynamic = registry
        .dynamic
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    installed.iter().all(|(id, installation)| {
        let Some(current) = dynamic.get(id) else {
            return false;
        };
        installation
            .installed
            .upgrade()
            .is_some_and(|installed| std::sync::Arc::ptr_eq(current, &installed))
    })
}

/// Demote one session's stamp to a cleanup-only record before a live pass.
/// The pass may replace the installed provider kind (`streamSimple` installs
/// a `JsStreamProvider`; `refreshModels` replaces the model list), so its
/// `fast_path` must stop short-circuiting: keeping it would let a later
/// reversion to the same plain definition keep the JS provider (P2-2).
///
/// The `installed` ownership records stay in place. A cancelled live pass
/// then still has a cleanup stamp for everything the session already
/// installed (SNAP6 P2-2), and a definition removed by the pass remains in
/// the record until teardown.
fn demote_provider_sync_stamp(scope: u64, session: usize) {
    let mut stamps = provider_sync_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(stamp) = stamps.get_mut(&(scope, session)) {
        stamp.fast_path = false;
    }
    retain_live_provider_sync_stamps(&mut stamps);
}

/// The ownership records one session's last pass left behind. Read before a
/// full pass so a partial/cancelled pass can carry forward entries whose
/// definitions were removed (SNAP6 P2-2).
fn provider_sync_session_installations(
    scope: u64,
    session: usize,
) -> Vec<(String, ProviderInstallation)> {
    let mut stamps = provider_sync_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_live_provider_sync_stamps(&mut stamps);
    stamps
        .get(&(scope, session))
        .map(|stamp| stamp.installed.clone())
        .unwrap_or_default()
}

/// Combine a pass's fresh installations with the session's previous ownership
/// records and the dynamic map as it was before the pass.
///
/// For a freshly installed id the displaced entry is the pre-pass `dynamic`
/// value. When that value is still the allocation this SAME session installed
/// in an earlier pass, keep the earlier record's `previous` (the true owner
/// before this session first took the id) instead of recording the session's
/// own stale wrapper. Otherwise the displaced entry is turned into a restore
/// chain that also carries the chain of the session that installed it, so
/// teardown can skip a sibling that has already ended (SNAP7 P2-3). Entries
/// not reinstalled in this pass (removed definitions, a cancelled partial
/// pass) are carried over unchanged so teardown can still find them.
fn merge_provider_installations(
    scope: u64,
    session: usize,
    dynamic_before: &std::collections::BTreeMap<String, crate::ai::provider::ProviderBox>,
    prior: &[(String, ProviderInstallation)],
    installed_now: &[(String, std::sync::Weak<dyn crate::ai::provider::Provider>)],
) -> Vec<(String, ProviderInstallation)> {
    let mut merged = Vec::with_capacity(prior.len().max(installed_now.len()));
    for (id, installed) in installed_now {
        let displaced = dynamic_before.get(id).cloned();
        let previous = match prior.iter().find(|(old_id, _)| old_id == id) {
            Some((_, old)) => {
                let displaced_is_ours = displaced.as_ref().is_some_and(|candidate| {
                    old.installed.upgrade().is_some_and(|old_installed| {
                        std::sync::Arc::ptr_eq(candidate, &old_installed)
                    })
                });
                if displaced_is_ours {
                    old.previous.clone()
                } else {
                    displaced
                        .as_ref()
                        .map(|candidate| provider_previous_chain(scope, session, id, candidate))
                        .unwrap_or_default()
                }
            }
            None => displaced
                .as_ref()
                .map(|candidate| provider_previous_chain(scope, session, id, candidate))
                .unwrap_or_default(),
        };
        merged.push((
            id.clone(),
            ProviderInstallation {
                installed: installed.clone(),
                previous,
            },
        ));
    }
    for (id, old) in prior {
        if !installed_now.iter().any(|(new_id, _)| new_id == id) {
            merged.push((id.clone(), old.clone()));
        }
    }
    merged
}

/// Persist the ownership record after one successful installation. The stamp
/// is cleanup-only (`fast_path: false`) until the pass completes, so the
/// record is available even if the pass is cancelled at a later `await`
/// (SNAP6 P2-2). The final successful pass overwrites it with the reusable
/// fast-path stamp.
fn persist_provider_sync_installations(
    session: &AgentSession,
    scope: u64,
    dynamic_before: &std::collections::BTreeMap<String, crate::ai::provider::ProviderBox>,
    prior: &[(String, ProviderInstallation)],
    installed_now: &[(String, std::sync::Weak<dyn crate::ai::provider::Provider>)],
) {
    let merged = merge_provider_installations(
        scope,
        provider_sync_session_identity(session),
        dynamic_before,
        prior,
        installed_now,
    );
    write_provider_sync_stamp(session, scope, 0, Vec::new(), false, merged);
}

/// Remove or restore the dynamic provider entries one session installed. Each
/// id is changed only while the shared registry still resolves it to the
/// exact allocation that session's last sync installed (SNAP6 P1-1).
///
/// Why this exists (SNAP4 review P2-1): a subagent child shares the parent's
/// `Arc<ProviderRegistry>` but runs its own `sync_plugin_providers`, which
/// replaces the parent's dynamic entries with child-built ones — including
/// `OAuthFlowOverlay`s bound (weakly) to the child's extension runtime. The
/// weak binding stops the registry from pinning the child actor, but the
/// entry itself survived the child's end, so the parent's `/login` or OAuth
/// credential refresh between the child's end and the parent's next prompt
/// still resolved the child's flow: executing the child's JS callback while
/// the child lives (wrong session/cwd) or failing with "the extension runtime
/// has exited" after it is gone.
///
/// Deleting the child's entry was not enough (SNAP5 review P1-1): the parent
/// session may issue another model request in the SAME prompt after a
/// subagent tool returns, and `sync_plugin_providers` only runs at the start
/// of `run_prompt`. The parent's provider had been overwritten in the shared
/// map, so deleting the child's entry left `provider_box` with nothing and
/// the request failed with `Unknown provider`. The stamp therefore records
/// the entry each installation displaced; cleanup restores that allocation
/// (the parent's/external owner's provider) and only deletes when there was
/// no previous owner.
///
/// The session's [`ProviderSyncStamp`] is the ownership record: its
/// `installed` list holds the `Weak`s this session's own registration passes
/// returned plus the strong `previous` allocations. Removing the stamp first
/// makes the cleanup one-shot; each entry is then changed through
/// [`crate::ai::providers::RegisterDynamicProviderShared::restore_dynamic_provider_shared_if_installed`],
/// whose `Arc` identity check leaves a later parent/sibling/external
/// registration of the same id untouched. Synchronous on purpose: the caller
/// is a `Drop` guard in the child teardown path.
pub(crate) fn cleanup_provider_sync_session(
    registry: &ProviderRegistry,
    scope: u64,
    session: usize,
    provider_session: &crate::ai::provider::ProviderSessionKey,
) {
    // Session-scoped overlay first: after this session ends no request may
    // resolve its dynamic/OAuth providers through the shared registry, even
    // when the process-shared map still temporarily holds them.
    registry.clear_session_dynamic_providers(provider_session);
    let installed = {
        let mut stamps = provider_sync_stamps()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let installed = stamps
            .remove(&(scope, session))
            .map(|stamp| stamp.installed)
            .unwrap_or_default();
        retain_live_provider_sync_stamps(&mut stamps);
        installed
    };
    for (id, installation) in installed {
        use crate::ai::providers::RegisterDynamicProviderShared as _;
        let previous = resolve_previous_provider(&id, &installation.previous);
        registry.restore_dynamic_provider_shared_if_installed(
            &id,
            &installation.installed,
            previous.as_ref(),
        );
    }
}

/// Synchronous cleanup for one session's dynamic provider entries, run when
/// the guard drops. See [`cleanup_provider_sync_session`].
struct ProviderSyncCleanupGuard {
    registry: Arc<ProviderRegistry>,
    scope: u64,
    session: usize,
    provider_session: crate::ai::provider::ProviderSessionKey,
}

impl Drop for ProviderSyncCleanupGuard {
    fn drop(&mut self) {
        // No await: `Drop` only touches entries whose exact `Arc` this
        // session installed. `ProviderSyncStamp` uses `Weak`s for the
        // installed allocation and a strong restore chain, so a provider that
        // was already replaced is left to its current owner while the nearest
        // still-live displaced owner is restored otherwise.
        cleanup_provider_sync_session(
            &self.registry,
            self.scope,
            self.session,
            &self.provider_session,
        );
    }
}

/// Store one session's provider-sync stamp. `fast_path == false` stamps exist
/// only so child teardown can find the allocations a live/partial pass
/// installed; they can never skip a later sync.
fn write_provider_sync_stamp(
    session: &AgentSession,
    scope: u64,
    definitions: u64,
    models: Vec<(String, u64)>,
    fast_path: bool,
    installed: Vec<(String, ProviderInstallation)>,
) {
    let mut stamps = provider_sync_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retain_live_provider_sync_stamps(&mut stamps);
    stamps.insert(
        (scope, provider_sync_session_identity(session)),
        ProviderSyncStamp {
            registry: std::sync::Arc::downgrade(&session.registry),
            session: provider_sync_session_handle(session),
            definitions,
            models,
            fast_path,
            installed,
        },
    );
}

/// Hash of the ordered `(id, config)` definition set with the `$ENV`
/// references `sync_plugin_providers` interpolates resolved. `None` when a
/// definition has no string id (the sync loop skips it, so the fast path must
/// not treat the set as stable).
fn provider_sync_signature(defs: &[JsonValue]) -> Option<u64> {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for def in defs {
        let id = def.get("id").and_then(JsonValue::as_str)?;
        id.hash(&mut hasher);
        let config = def.get("config").cloned().unwrap_or(JsonValue::Null);
        provider_config_fingerprint(&config).hash(&mut hasher);
        // `apiKey` and `headers` values expand `$VAR` / `${VAR}` from the
        // process environment at registration time; an env change must re-run
        // the pass even when the raw definition is byte-identical.
        if let Some(key) = config.get("apiKey").and_then(JsonValue::as_str) {
            interpolate_api_key(key).hash(&mut hasher);
        }
        if let Some(headers) = config.get("headers").and_then(JsonValue::as_object) {
            for (name, value) in headers {
                name.hash(&mut hasher);
                value.as_str().map(interpolate_api_key).hash(&mut hasher);
            }
        }
    }
    Some(hasher.finish())
}

/// `(id, model content hash)` for the registry state each definition should
/// have produced. `Debug` is a deterministic, complete dump of the plain
/// `Model` data (all collections are ordered maps); only the hash is kept.
fn provider_sync_models(session: &AgentSession, defs: &[JsonValue]) -> Vec<(String, u64)> {
    use std::hash::{Hash as _, Hasher as _};

    defs.iter()
        .filter_map(|def| def.get("id").and_then(JsonValue::as_str))
        .map(|id| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for model in session.registry.get_provider_models(id) {
                format!("{model:?}").hash(&mut hasher);
            }
            (id.to_owned(), hasher.finish())
        })
        .collect()
}

/// Whether a definition needs the synchronous per-prompt pass even when its
/// config is unchanged: `refreshModels` owns a time window evaluated inside the
/// loop, and `streamSimple` binds the provider to the live JS handle.
fn provider_def_needs_live_pass(def: &JsonValue) -> bool {
    def.get("config").is_some_and(|config| {
        config.get("refreshModels").and_then(JsonValue::as_bool) == Some(true)
            || config.get("streamSimple").and_then(JsonValue::as_bool) == Some(true)
    })
}

fn mark_refresh_models(scope: usize, id: &str, fingerprint: u64, models: Option<JsonValue>) {
    refresh_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            refresh_stamp_key(scope, id),
            RefreshStamp {
                fingerprint,
                last_refresh: std::time::Instant::now(),
                models,
            },
        );
}

/// The last successful refresh's model list for `id`, when the stamp belongs
/// to the same definition. `None` after a failed/empty refresh (or a changed
/// definition), so the caller falls back to the persisted catalog / registry.
fn cached_refresh_models(scope: usize, id: &str, fingerprint: u64) -> Option<JsonValue> {
    refresh_stamps()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&refresh_stamp_key(scope, id))
        .filter(|stamp| stamp.fingerprint == fingerprint)
        .and_then(|stamp| stamp.models.clone())
}

/// Parse a wire `models` array against the provider's `baseUrl`/`api` (the
/// same shape the refresh result is parsed with). `None` when the list has no
/// usable entries.
fn parse_refreshed_models(config: &JsonValue, models: &JsonValue) -> Option<Vec<Model>> {
    let refreshed_config = serde_json::json!({
        "baseUrl": config.get("baseUrl").cloned().unwrap_or(JsonValue::Null),
        "api": config.get("api").cloned().unwrap_or(JsonValue::Null),
        "models": models,
    });
    parse_provider_models(&refreshed_config)
        .ok()
        .filter(|models| !models.is_empty())
}

impl AgentSession {
    /// Arm a `Drop` guard that restores (or removes) the dynamic provider
    /// entries THIS session installed in its (possibly parent-shared)
    /// registry when the guard is dropped. The subagent child holds it for the
    /// whole run so a cancelled `run_child` future cleans up too, not only the
    /// normal `shutdown` tail.
    ///
    /// Each entry is changed only while it is still identical to the `Weak`
    /// recorded by this session's last provider sync; a parent/sibling that
    /// re-registered the id afterwards keeps its entry. When the entry is
    /// still ours, the nearest still-live provider in the captured restore
    /// chain is put back instead of deleting the id, so the parent's next
    /// model request in the same prompt keeps resolving its own provider
    /// (SNAP6 P1-1). Entries whose owner session has already torn down are
    /// skipped (SNAP7 P2-3).
    pub(crate) fn provider_sync_cleanup_guard(&self) -> impl Drop {
        ProviderSyncCleanupGuard {
            registry: Arc::clone(&self.registry),
            scope: self.registry.refresh_scope_id(),
            session: provider_sync_session_identity(self),
            provider_session: self.provider_session_key(),
        }
    }

    /// `/login` for THIS session: resolves the provider through the session
    /// overlay before the shared dynamic map, so a live subagent child's
    /// OAuth flow (wrong session/cwd) is never executed (SNAP8 P2-1).
    pub async fn login_provider(
        &self,
        provider_id: &str,
        auth_type: crate::ai::types::AuthType,
        interaction: &dyn crate::ai::auth::AuthInteraction,
        options: &crate::ai::auth::LoginOptions,
    ) -> crate::ai::error::AiResult<crate::plugins::services::ProviderAuth> {
        self.registry
            .login_for_session(
                &self.provider_session_key(),
                provider_id,
                auth_type,
                interaction,
                options,
            )
            .await
    }

    /// Model/credential refresh for THIS session: provider lookup (and the
    /// OAuth credential refresh inside it) uses the session overlay first
    /// (SNAP8 P2-1).
    pub async fn refresh_provider_models(
        &self,
        options: crate::ai::provider::ModelsRefreshOptions,
    ) -> crate::ai::provider::ModelsRefreshResult {
        self.registry
            .refresh_for_session(&self.provider_session_key(), options)
            .await
    }

    /// The OAuth flow of one provider as THIS session sees it (SNAP10
    /// P2-S8-2). The TUI uses it for `/login` rows and subscription labels:
    /// a live child's overlay entry can never answer for the parent, while a
    /// session-less caller keeps [`crate::ai::auth::oauth_flow_for`]'s static
    /// behavior.
    pub fn oauth_flow_for_provider(
        &self,
        provider_id: &str,
    ) -> Option<crate::ai::auth::OAuthFlowBox> {
        self.registry
            .provider_box_for_session_for_test(&self.provider_session_key(), provider_id)
            .and_then(|provider| provider.oauth_flow())
    }

    /// Resolve a provider's request auth through THIS session (SNAP10
    /// P2-S8-2): `ProviderRegistry::get_auth_for_session` uses the session
    /// overlay for the provider/OAuth flow and refreshes an expiring stored
    /// credential inside the store lock. Used by the TUI `/share` Radius token
    /// path; `None`/`Err` means the provider is unknown or unconfigured.
    pub async fn get_auth_for_provider(
        &self,
        provider_id: &str,
    ) -> crate::ai::error::AiResult<Option<crate::ai::types::AuthResult>> {
        let session = self.provider_session_key();
        let model = self
            .registry
            .get_provider_models_for_session(&session, provider_id)
            .into_iter()
            .next()
            .or_else(|| {
                (self.current_model().provider == provider_id).then(|| self.current_model())
            })
            .ok_or_else(|| crate::ai::error::AiError::UnknownProvider(provider_id.to_string()))?;
        self.registry.get_auth_for_session(&session, &model).await
    }

    /// `(provider, model)` pairs visible to THIS session, for the TUI model
    /// selectors (SNAP10 P2-S8-2). Provider ids come from the live registry
    /// (catalog + plugin/dynamic entries); each list is resolved through the
    /// session overlay, so a child's same-id provider cannot replace the
    /// parent's models. Virtual overlay entries are appended like
    /// `ProviderRegistry::get_models`.
    pub fn provider_model_pairs(&self) -> Vec<(String, String)> {
        let session = self.provider_session_key();
        let mut pairs = std::collections::BTreeSet::new();
        for provider in self.registry.providers() {
            let id = provider.id().to_string();
            for model in self.registry.get_provider_models_for_session(&session, &id) {
                pairs.insert((model.provider, model.id));
            }
        }
        for registration in self.registry.virtual_models() {
            pairs.insert((registration.provider, registration.id));
        }
        pairs.into_iter().collect()
    }

    /// Test seam for the child-teardown cleanup guard: dropping the returned
    /// value runs exactly the synchronous cleanup `run_child` performs when
    /// its child session ends (SNAP4 review P2-1 / SNAP6 P1-1).
    #[doc(hidden)]
    pub fn provider_sync_cleanup_guard_for_test(&self) -> impl Drop {
        self.provider_sync_cleanup_guard()
    }

    /// Convert `pi.registerProvider` definitions 鈥?from the unified plugin
    /// registry (JS extensions and Rust builtins alike) 鈥?into real registry
    /// providers. Idempotent: definitions are upserted by provider id, and an
    /// unchanged definition set with unchanged installed model content short-
    /// circuits before any dynamic provider `Arc` is rebuilt. Async: a
    /// `refreshModels` hook (upstream
    /// `ProviderConfig.refreshModels`) refreshes the model list through a
    /// bounded JS round trip at registration.
    pub(super) async fn sync_plugin_providers(&self, plugins: &PluginHost) {
        let defs = plugins.registry().provider_defs();
        // Idempotent fast path (P2-新-1): re-applying an unchanged definition
        // set used to rebuild every dynamic provider `Arc` on every prompt,
        // which invalidated the session catalog cache by address even though
        // the catalog was identical. The stamp is scoped to this session
        // (registry scope + session identity) and records both the resolved
        // definition set and the model content it installed; a changed
        // definition, a changed credential env value, or
        // an external provider replacement/removal falls through to the full
        // loop. Definitions with per-prompt semantics (`refreshModels` window,
        // `streamSimple` JS handle) always take the full loop.
        let needs_live_pass = defs.iter().any(provider_def_needs_live_pass);
        let sync_signature = provider_sync_signature(&defs);
        let scope = self.registry.refresh_scope_id();
        let session = provider_sync_session_identity(self);
        let provider_session = self.provider_session_key();
        // Keep the previous pass's ownership records (including the entries it
        // displaced) available to the full pass: a partial/cancelled pass must
        // still leave a cleanup record for definitions it does not reach, and
        // a removed definition must stay removable at teardown (SNAP6 P2-2).
        let prior_installations = provider_sync_session_installations(scope, session);
        if needs_live_pass {
            // A live pass installs per-prompt state (`refreshModels` window,
            // `streamSimple` JS handle) and may not write a fast-path stamp.
            // Demote any stamp from an earlier plain pass so a later reversion
            // to the same plain definition cannot short-circuit and leave the
            // JS-stream provider installed (P2-2). The pass keeps the
            // installed ownership records and writes cleanup stubs as it
            // installs, so a cancelled pass is still reclaimable by child
            // teardown.
            demote_provider_sync_stamp(scope, session);
        } else if let Some(signature) = sync_signature {
            let models = provider_sync_models(self, &defs);
            let candidate = {
                let mut stamps = provider_sync_stamps()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Prune dead sessions before the lookup: a dead child's stamp
                // must not be observable even if the allocator reuses its
                // identity address for a new child of the same shared registry
                // (the Weak still reports zero strong references for the old
                // allocation).
                retain_live_provider_sync_stamps(&mut stamps);
                stamps.get(&(scope, session)).map(|stamp| {
                    (
                        stamp.definitions,
                        stamp.models.clone(),
                        stamp.installed.clone(),
                        stamp.fast_path,
                    )
                })
            };
            // The dynamic map is shared across sessions; a matching stamp only
            // means this session's last pass installed the same definitions,
            // not that its allocations are still the ones the registry
            // resolves. A subagent child's pass replaces them (and binds its
            // own OAuth flow), so verify the exact `Arc`s before short-
            // circuiting (SNAP4 P2-新-1). The check runs after dropping the
            // stamp lock to keep the lock order off the dynamic-map lock.
            if let Some((stamped_signature, stamped_models, installed, fast_path)) = candidate {
                if fast_path
                    && stamped_signature == signature
                    && stamped_models == models
                    && provider_sync_installations_match(&self.registry, &installed)
                {
                    return;
                }
            }
        }
        // The shared map as it is BEFORE this pass installs anything: the
        // entry each successful registration displaces is the `previous` that
        // child teardown restores (SNAP6 P1-1).
        let dynamic_before = self
            .registry
            .dynamic
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut applied_all = true;
        // The providers THIS pass installed, as identity witnesses for the
        // fast path. Kept per pass (not snapshotted from the shared map): the
        // map is last-writer-wins across sessions sharing this registry, so a
        // post-pass read could observe a sibling's replacement.
        let mut installed: Vec<(String, std::sync::Weak<dyn crate::ai::provider::Provider>)> =
            Vec::new();
        for def in &defs {
            let Some(id) = def.get("id").and_then(JsonValue::as_str) else {
                applied_all = false;
                continue;
            };
            let config = def.get("config").cloned().unwrap_or(JsonValue::Null);
            // Provider-level `headers`/`authHeader` (upstream ProviderConfig
            // headers/authHeader, #22): provider headers merge under per-model
            // headers (model entries win), and `authHeader: true` stamps
            // `Authorization: Bearer <key>` with the resolved API key
            // (upstream withConfiguredAuth). The merge runs AFTER the refresh
            // below: `refreshModels` replaces the model list wholesale, so
            // merging only the configured entries would drop the provider's
            // Authorization/custom headers from every refreshed model.
            let provider_headers = apply_provider_headers(&config);
            let auth_header = config
                .get("authHeader")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let mut models = match parse_provider_models(&config) {
                Ok(models) => models,
                Err(error) => {
                    crate::ext_diag!("[ext] provider `{id}` skipped: {error}");
                    applied_all = false;
                    continue;
                }
            };
            let resolved_key = config
                .get("apiKey")
                .and_then(JsonValue::as_str)
                .map(interpolate_api_key)
                .filter(|key| !key.is_empty());
            if auth_header && resolved_key.is_none() {
                crate::ext_diag!(
                    "[ext] provider `{id}`: authHeader requires a resolved API key; requests will fail auth"
                );
            }
            // `refreshModels: true` marker (upstream
            // `ProviderConfig.refreshModels`, types.ts:1941): the registered
            // JS callback refreshes the model list through the
            // `provider:refreshModels` synthetic event; a non-empty result
            // replaces the configured models (upstream refreshModels
            // semantics).
            if config.get("refreshModels").and_then(JsonValue::as_bool) == Some(true) {
                let fingerprint = provider_config_fingerprint(&config);
                let stamp_scope = self.registry.refresh_scope_id() as usize;
                if !refresh_models_due(stamp_scope, id, fingerprint) {
                    // Inside the window with an unchanged definition: do not
                    // pay the network refresh again, and do not overwrite the
                    // previously refreshed list with the configured (stale)
                    // one. The cached list is the refresh's own outcome; a
                    // session whose registry was never populated (a second
                    // session in this process sharing the stamp) restores it
                    // instead of falling through to the configured models.
                    let cached = cached_refresh_models(stamp_scope, id, fingerprint)
                        .and_then(|models| parse_refreshed_models(&config, &models));
                    let restored = cached.or_else(|| {
                        read_provider_catalog(self, id)
                            .and_then(|entry| restore_models_from_store(&config, id, &entry))
                            .or_else(|| {
                                let existing = self.registry.get_provider_models(id);
                                (!existing.is_empty()).then_some(existing)
                            })
                    });
                    if let Some(restored) = restored {
                        models = restored;
                    }
                } else {
                    // The persisted catalog is the provider-scoped snapshot the
                    // refresh callback sees as `context.stored` (upstream
                    // `ModelsStore.read` -> `RefreshModelsContext.stored`). It is
                    // also the cross-restart fallback when the refresh yields no
                    // usable models (offline start / callback failure).
                    //
                    // The wire copy exists only for the JS refresh callback. A host
                    // whose dispatch has no JS handler for the synthetic event (a
                    // builtin-only plan, or a native provider definition that never
                    // installed a JS callback) never reads `context.stored`, so the
                    // store is not even read here; the restore paths below read it
                    // on demand, and the store's stamp cache makes a repeat read a
                    // metadata probe rather than a file parse.
                    let js_reads_stored = plugins.has_handlers("provider:refreshModels");
                    let mut payload = serde_json::json!({ "providerId": id });
                    if js_reads_stored {
                        if let Some(entry) = read_provider_catalog(self, id) {
                            // The entry is owned here, so move its model list into
                            // the payload instead of deep-copying it.
                            payload["stored"] = entry.into_wire();
                        }
                    }
                    // The wire list a successful refresh produced. Cached on the
                    // stamp so a later session inside the window restores the
                    // refreshed models instead of the configured (stale) ones;
                    // a failed/empty refresh caches `None` (there is no fresh
                    // list to serve).
                    let mut refreshed_wire: Option<JsonValue> = None;
                    match plugins.emit("provider:refreshModels", &mut payload).await {
                        Ok(outcome) => {
                            // `publish({persist})` persistence (upstream
                            // `ModelsPublication.persist`): `null` deletes the
                            // provider entry, an object upserts it. The JS side
                            // records the last publication on the event outcome;
                            // writing it here keeps ModelsStore the single owner
                            // of the file lock.
                            let mut persisted_in_refresh = false;
                            if let Some(publication) = outcome.result.get("persist") {
                                persisted_in_refresh = true;
                                persist_provider_catalog(self, id, publication);
                            }
                            let refreshed_config = serde_json::json!({
                                "baseUrl": config.get("baseUrl").cloned().unwrap_or(JsonValue::Null),
                                "api": config.get("api").cloned().unwrap_or(JsonValue::Null),
                                "models": outcome
                                    .result
                                    .get("models")
                                    .cloned()
                                    .unwrap_or(JsonValue::Null),
                            });
                            match parse_provider_models(&refreshed_config) {
                                Ok(refreshed) if !refreshed.is_empty() => {
                                    refreshed_wire = outcome.result.get("models").cloned();
                                    models = refreshed;
                                }
                                _ => {
                                    // Keep the persisted catalog when the refresh
                                    // returned nothing usable (the callback may be
                                    // offline or have failed). A publish that
                                    // already provided models is authoritative, so
                                    // it does not fall back to the pre-refresh
                                    // stored snapshot.
                                    let restored = (!persisted_in_refresh)
                                        .then(|| {
                                            read_provider_catalog(self, id).and_then(|entry| {
                                                restore_models_from_store(&config, id, &entry)
                                            })
                                        })
                                        .flatten();
                                    if let Some(restored) = restored {
                                        models = restored;
                                    } else {
                                        crate::ext_diag!(
                                        "[ext] provider `{id}`: refreshModels returned no usable models; keeping the configured list"
                                    );
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            // Dispatch failure is not persisted away: restore the
                            // cached catalog when one exists.
                            let restored = read_provider_catalog(self, id)
                                .and_then(|entry| restore_models_from_store(&config, id, &entry));
                            let restored_from_store = restored.is_some();
                            if let Some(restored) = restored {
                                models = restored;
                            }
                            crate::ext_diag!(
                                "[ext] provider `{id}`: refreshModels dispatch failed: {error}{}",
                                if restored_from_store {
                                    "; restored the persisted catalog"
                                } else {
                                    "; keeping the configured list"
                                }
                            );
                        }
                    }
                    mark_refresh_models(stamp_scope, id, fingerprint, refreshed_wire);
                }
            }
            // Re-apply the configured provider/model headers and authHeader to
            // whichever list won (configured, refreshed, or restored).
            apply_configured_headers(
                &mut models,
                provider_headers.as_ref(),
                auth_header,
                resolved_key.as_deref(),
            );
            // `streamSimple: true` marker (upstream `ProviderConfig.streamSimple`):
            // the provider streams through the extension's JS callback via the
            // reserved `__pir_stream:` hook channel (providers/js_stream.rs).
            if config.get("streamSimple").and_then(JsonValue::as_bool) == Some(true) {
                let Some(js) = plugins.js_handle().cloned() else {
                    crate::ext_diag!(
                        "[ext] provider `{id}` skipped: streamSimple requires the JS extension runtime"
                    );
                    applied_all = false;
                    continue;
                };
                match crate::ai::providers::RegisterJsStreamProviderShared::register_js_stream_provider_shared(
                    self.registry.as_ref(),
                    id.to_string(),
                    models,
                    js,
                ) {
                    Ok(registered) => {
                        // The live pass writes only a cleanup-only stamp, but
                        // it must record this installation for child teardown.
                        record_provider_installation(
                            &self.registry,
                            &provider_session,
                            &mut installed,
                            id,
                            &registered,
                        );
                        persist_provider_sync_installations(
                            self,
                            scope,
                            &dynamic_before,
                            &prior_installations,
                            &installed,
                        );
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] provider `{id}` registration failed: {error}");
                        applied_all = false;
                    }
                }
                continue;
            }
            if models.is_empty() {
                // Upstream `applyExtension` (provider-composer.ts:341-351): a
                // model-less definition with a `baseUrl` OVERRIDES the URL for
                // the provider's existing models (e.g. rewriting the anthropic
                // baseUrl); the dynamic overlay wins over the static registry
                // entry on every lookup, and unregistering restores the
                // built-in. Without a `baseUrl` the definition carries no
                // applicable change.
                let Some(base_url) = config
                    .get("baseUrl")
                    .and_then(JsonValue::as_str)
                    .filter(|url| !url.is_empty())
                else {
                    // Nothing was installed for this definition, so this pass
                    // must not write a stamp that claims every definition is
                    // fully applied (review-snap3 P3-5).
                    applied_all = false;
                    continue;
                };
                let existing = self.registry.get_provider_models(id);
                if existing.is_empty() {
                    crate::ext_diag!(
                        "[ext] provider `{id}` skipped: baseUrl-only override but the provider has no known models"
                    );
                    applied_all = false;
                    continue;
                }
                let overridden: Vec<crate::ai::types::Model> = existing
                    .into_iter()
                    .map(|mut model| {
                        model.base_url = base_url.to_string();
                        model
                    })
                    .collect();
                let dynamic = DynamicProviderConfig {
                    base_url: base_url.to_string(),
                    models: overridden,
                    ..DynamicProviderConfig::default()
                };
                match self
                    .registry
                    .register_dynamic_provider_shared(id.to_string(), dynamic)
                {
                    Ok(registered) => {
                        record_provider_installation(
                            &self.registry,
                            &provider_session,
                            &mut installed,
                            id,
                            &registered,
                        );
                        persist_provider_sync_installations(
                            self,
                            scope,
                            &dynamic_before,
                            &prior_installations,
                            &installed,
                        );
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] provider `{id}` baseUrl override failed: {error}");
                        applied_all = false;
                    }
                }
                continue;
            }
            // The wire API id routes through `build_provider_from_models`
            // (providers/construction.rs), which maps `google-generative-ai`
            // to the dedicated Google adapter — the same adapter the built-in
            // catalog provider uses (upstream routes by model.api).
            let api = config
                .get("api")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let dynamic = DynamicProviderConfig {
                base_url: config
                    .get("baseUrl")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                api_key: config
                    .get("apiKey")
                    .and_then(JsonValue::as_str)
                    .map(interpolate_api_key),
                api,
                models,
                compat: None,
                refresh: None,
            };
            match self
                .registry
                .register_dynamic_provider_shared(id.to_string(), dynamic)
            {
                Ok(registered) => {
                    let mut installed_provider = registered;
                    if let Some(api_key) = config
                        .get("apiKey")
                        .and_then(JsonValue::as_str)
                        .map(interpolate_api_key)
                        .filter(|key| !key.is_empty())
                    {
                        self.extension_api_keys
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id.to_string(), api_key);
                    }
                    // Upstream ProviderConfig.oauth (#12): attach the
                    // extension's OAuth flow so `/login` and the auth-status
                    // surfaces discover it through `Provider::oauth_flow` (the
                    // flow executes the JS login/refresh callbacks; the /login
                    // TUI seam drives it via the standard flow interface).
                    if config
                        .get("oauth")
                        .and_then(|oauth| oauth.get("login"))
                        .and_then(JsonValue::as_bool)
                        == Some(true)
                    {
                        if let Some(js) = plugins.js_handle() {
                            let display_name = config
                                .get("oauth")
                                .and_then(|oauth| oauth.get("name"))
                                .and_then(JsonValue::as_str)
                                .unwrap_or(id)
                                .to_string();
                            let is_subscription = config
                                .get("oauth")
                                .and_then(|oauth| oauth.get("isSubscription"))
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false);
                            // Bind the flow to this session's runtime through a
                            // WEAK handle (SNAP4 P2-新-1): the overlay is kept
                            // in the (possibly parent-shared) registry, and a
                            // strong `ExtensionHandle` clone here used to pin
                            // the child's actor/QuickJS runtime after the
                            // child session and its `ChildRuntime` dropped.
                            // The flow re-binds a fresh `JsOAuthFlow` per
                            // login/refresh and fails cleanly once the owning
                            // runtime is gone.
                            if let Some(flow) =
                                js.weak_oauth_flow(id.to_string(), display_name, is_subscription)
                            {
                                // Wrap the provider this pass registered, not
                                // whatever the shared map holds now (another
                                // session may have replaced it between the
                                // registration and this wrap).
                                let wrapped = crate::ai::providers::js_stream::with_oauth_flow(
                                    installed_provider.clone(),
                                    flow,
                                );
                                self.registry
                                    .dynamic
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(id.to_string(), wrapped.clone());
                                // The stamp must witness the overlay actually
                                // left in the map, not the inner provider it
                                // wraps; both are ours, but only the overlay
                                // is what the fast-path lookup compares.
                                installed_provider = wrapped;
                            }
                        }
                    }
                    record_provider_installation(
                        &self.registry,
                        &provider_session,
                        &mut installed,
                        id,
                        &installed_provider,
                    );
                    persist_provider_sync_installations(
                        self,
                        scope,
                        &dynamic_before,
                        &prior_installations,
                        &installed,
                    );
                }
                Err(error) => {
                    crate::ext_diag!("[ext] provider `{id}` registration failed: {error}");
                    applied_all = false;
                }
            }
        }
        // A fully-applied plain pass writes the reusable fast-path stamp. Any
        // other pass that installed providers (live `streamSimple`/
        // `refreshModels`, or a pass with a skipped definition) still writes a
        // cleanup-only stamp: it records the allocations this session owns so
        // child teardown can restore/remove them, while `fast_path: false`
        // guarantees it can never short-circuit a later sync. Both branches
        // carry prior ownership records for definitions this pass did not
        // reinstall (removed definitions, cancelled partial passes).
        let final_installations = merge_provider_installations(
            scope,
            session,
            &dynamic_before,
            &prior_installations,
            &installed,
        );
        // A fully-applied non-live pass is the authoritative definition set:
        // drop overlay entries whose definition disappeared (the legacy
        // cleanup stamp still carries them until teardown for the shared map,
        // but a live session must resolve the static/global fallback again).
        if applied_all && !needs_live_pass {
            let keep: Vec<String> = installed.iter().map(|(id, _)| id.clone()).collect();
            self.registry
                .retain_session_dynamic_providers(&provider_session, &keep);
        }
        let fast_path_eligible = applied_all && !needs_live_pass;
        match (fast_path_eligible, sync_signature) {
            (true, Some(signature)) => {
                let models = provider_sync_models(self, &defs);
                write_provider_sync_stamp(
                    self,
                    scope,
                    signature,
                    models,
                    true,
                    final_installations,
                );
            }
            _ if !final_installations.is_empty() => {
                write_provider_sync_stamp(self, scope, 0, Vec::new(), false, final_installations);
            }
            _ => {}
        }
    }

    /// Apply `pi.registerVirtualModel` definitions (unified plugin registry:
    /// JS extensions and Rust plugins alike) to the provider registry's
    /// virtual-model overlay. Idempotent: the plugin-sourced slice is
    /// re-synced each prompt, and entries whose plugin went away are dropped.
    /// New registrations persist as `pi.virtual-model-state` custom entries so
    /// they survive resume (upstream persists router state on the branch).
    pub(super) async fn sync_plugin_virtual_models(&self, plugins: &PluginHost) {
        let provider_of = |def: &JsonValue| -> String {
            def.get("provider")
                .and_then(JsonValue::as_str)
                .or_else(|| {
                    def.get("physical")
                        .and_then(|physical| physical.get("provider"))
                        .and_then(JsonValue::as_str)
                })
                .unwrap_or_default()
                .to_owned()
        };
        let defs = plugins.registry().virtual_model_defs();
        let desired: std::collections::BTreeSet<(String, String)> = defs
            .iter()
            .map(|def| {
                (
                    provider_of(def),
                    def.get("id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect();
        for stale in self.registry.virtual_model_registry().all() {
            if stale.source.starts_with("plugin:")
                && !desired.contains(&(stale.provider.clone(), stale.id.clone()))
            {
                self.registry
                    .unregister_virtual_model(&stale.provider, &stale.id);
            }
        }
        for def in &defs {
            let source = def
                .get("source")
                .and_then(JsonValue::as_str)
                .unwrap_or("plugin");
            let registration =
                match crate::ai::virtual_::VirtualModelRegistration::from_wire(def, source) {
                    Ok(registration) => registration,
                    Err(error) => {
                        crate::ext_diag!("[ext] virtual model definition skipped: {error}");
                        continue;
                    }
                };
            let known = self
                .registry
                .virtual_model_registry()
                .is_registered(&registration.provider, &registration.id);
            if let Err(error) = self.registry.register_virtual_model(registration.clone()) {
                crate::ext_diag!(
                    "[ext] virtual model `{}` registration failed: {error}",
                    registration.key()
                );
                continue;
            }
            // Install/drop the per-request route callback (`route: true`
            // marker set by the JS prelude; upstream
            // `VirtualModelDefinition.route`). The callback round-trips to the
            // JS hook through the reserved `__pir_route:` channel
            // (providers/js_stream.rs); failures fall back to the static
            // route at request time.
            let route_callback = if def.get("route").and_then(JsonValue::as_bool) == Some(true) {
                plugins.js_handle().cloned().map(|js| {
                    crate::ai::providers::js_virtual_route_callback(
                        js,
                        registration.provider.clone(),
                        registration.id.clone(),
                    )
                })
            } else {
                None
            };
            self.registry.virtual_model_registry().set_route_callback(
                &registration.provider,
                &registration.id,
                route_callback,
            );
            if !known {
                self.persist_virtual_model_state(&registration).await;
            }
        }
    }

    /// Persist a virtual-model registration as a `pi.virtual-model-state`
    /// custom entry (data `{provider, modelId, state: definition}`), skipping
    /// an identical entry already on the branch.
    pub(super) async fn persist_virtual_model_state(
        &self,
        registration: &crate::ai::virtual_::VirtualModelRegistration,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let Ok(definition) = serde_json::to_value(registration) else {
            return;
        };
        let mut session = session.lock().await;
        // Borrowed lookup: `entries()` clones every entry (the whole
        // transcript) just to compare custom data. The resident list is the
        // same sequence `entries()` covers, and a header can never be the
        // custom entry being searched for.
        let exists = session.resident_entries().iter().any(|entry| {
            matches!(
                entry.as_ref(),
                crate::session_manager::FileEntry::Custom(custom)
                    if custom.custom_type == crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY
                        && custom.data.as_ref().and_then(|data| data.get("state"))
                            == Some(&definition)
            )
        });
        if exists {
            return;
        }
        let data = serde_json::json!({
            "provider": registration.provider,
            "modelId": registration.id,
            "state": definition,
        });
        // Upstream appends the route/registration state entry and emits
        // `entry_appended` for the persisted entry (agent-session.ts:830-834).
        let appended = match session
            .append_custom_entry(crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY, Some(data))
        {
            Ok(entry_id) => session
                .entry(&entry_id)
                .and_then(|entry| serde_json::to_value(entry).ok()),
            Err(error) => {
                crate::ext_diag!("[ext] failed to persist virtual-model state: {error}");
                None
            }
        };
        // Never dispatch a plugin event under the storage lock.
        drop(session);
        if let Some(entry) = appended {
            crate::agent_session::notifications::emit_entry_appended_wire(
                self.plugins.as_deref(),
                &self.session_events,
                entry,
            )
            .await;
        }
    }
    /// The session's internal `prepare_request` projection (upstream
    /// `_installAgentRequestProjection`): the host hook runs first, then a
    /// virtual selection routes the request to its physical model — the
    /// physical model handles the request while messages/usage keep the
    /// VIRTUAL identity. A registration carrying a `systemPrompt` prepends it
    /// to the request's system prompt. `routed_registration` records the
    /// registration the last request routed under.
    pub(super) fn virtual_routing_prepare_request(
        &self,
        routed_registration: Arc<
            std::sync::Mutex<Option<crate::ai::virtual_::VirtualModelRegistration>>,
        >,
    ) -> Option<PrepareRequestHook> {
        let host_hook = self
            .prepare_request
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let registry = Arc::clone(&self.registry);
        Some(Arc::new(
            move |mut request: agent_loop::PrepareRequestContext| {
                let registry = Arc::clone(&registry);
                let routed_registration = Arc::clone(&routed_registration);
                let host_hook = host_hook.clone();
                Box::pin(async move {
                    // Host hook first (upstream `previousPrepareRequest`); its
                    // update must survive even when no routing happens. The
                    // context is SHARED through the hook chain (`Arc`), so a
                    // transcript-sized context is not re-cloned per request.
                    let mut updated = false;
                    if let Some(host) = host_hook {
                        if let Some(update) = host(agent_loop::PrepareRequestContext {
                            context: Arc::clone(&request.context),
                            model: request.model.clone(),
                            thinking_level: request.thinking_level,
                        })
                        .await
                        {
                            if let Some(context) = update.context {
                                request.context = context;
                            }
                            if let Some(model) = update.model {
                                request.model = Some(model);
                            }
                            if let Some(level) = update.thinking_level {
                                request.thinking_level = level;
                            }
                            updated = true;
                        }
                    }
                    // Virtual routing: only a virtual selection routes. The
                    // projection is shared with `ProviderService::complete`
                    // (`resolve_virtual_request`), so an explicit completion
                    // and a session prompt produce the same physical route,
                    // route-callback decision and system prompt.
                    let model = request.model.clone()?;
                    if crate::ai::virtual_::is_virtual_model(&model) {
                        let thinking_level = request.thinking_level.to_string();
                        match resolve_virtual_request(
                            &registry,
                            &model,
                            &mut request.context,
                            Some(&thinking_level),
                        )
                        .await
                        {
                            Ok(Some((registration, physical))) => {
                                *routed_registration
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(registration);
                                return Some(agent_loop::PrepareRequestUpdate {
                                    // Shared through the chain: a refcount bump,
                                    // not a transcript-sized copy.
                                    context: Some(Arc::clone(&request.context)),
                                    model: Some(physical),
                                    thinking_level: Some(request.thinking_level),
                                });
                            }
                            Ok(None) => {
                                crate::ext_diag!(
                                    "[ext] virtual model {}/{} is not registered; the request will fail to stream",
                                    model.provider, model.id
                                );
                            }
                            Err(error) => {
                                crate::ext_diag!("[ext] virtual model routing failed: {error}");
                            }
                        }
                    }
                    // No routing: surface a host-hook update when one applied.
                    updated.then_some(agent_loop::PrepareRequestUpdate {
                        context: Some(request.context),
                        model: request.model,
                        thinking_level: Some(request.thinking_level),
                    })
                })
            },
        ))
    }

    /// Register a virtual model (upstream `registerVirtualModel`; embedder
    /// seam). Returns `Err` for invalid definitions and physical-id
    /// conflicts.
    pub fn register_virtual_model(
        &self,
        registration: crate::ai::virtual_::VirtualModelRegistration,
    ) -> Result<(), String> {
        self.registry.register_virtual_model(registration)
    }

    /// Remove a virtual model (upstream `unregisterVirtualModel`). Returns
    /// whether one was registered.
    pub fn unregister_virtual_model(&self, provider: &str, id: &str) -> bool {
        self.registry.unregister_virtual_model(provider, id)
    }

    /// The registered virtual models.
    pub fn virtual_models(&self) -> Vec<crate::ai::virtual_::VirtualModelRegistration> {
        self.registry.virtual_models()
    }

    /// A physical (non-virtual) catalog model (upstream `getPhysicalModel`).
    pub fn get_physical_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.registry.get_physical_model(provider, id)
    }
}

// ---------------------------------------------------------------------------
// Plugin event helpers
// ---------------------------------------------------------------------------

/// Resolve a virtual selection to the physical model that handles the request
/// (upstream `ModelRuntime.resolveModel`, static-routing form): the physical
/// model must exist in the catalog and not itself be virtual. Returns
/// `Ok(None)` when the virtual model is not registered (the request then
/// fails at the provider with "must be routed").
fn route_virtual_model(
    registry: &ProviderRegistry,
    model: &Model,
) -> Result<Option<(crate::ai::virtual_::VirtualModelRegistration, Model)>, String> {
    let Some(registration) = registry
        .virtual_model_registry()
        .get(&model.provider, &model.id)
    else {
        return Ok(None);
    };
    let physical = registry
        .get_physical_model(&registration.physical.provider, &registration.physical.id)
        .ok_or_else(|| {
            format!(
                "virtual model {} routes to {}/{}, which is not a physical model",
                registration.key(),
                registration.physical.provider,
                registration.physical.id
            )
        })?;
    Ok(Some((registration, physical)))
}

/// The one virtual-selection request projection shared by the session's
/// `prepare_request` hook and `ProviderService::complete`.
///
/// It resolves the static physical route, consults the per-request `route`
/// callback (falling back to the static route on a callback error or an
/// invalid target), and prepends the registration's `systemPrompt` to the
/// request context. `thinking_level` is the request's selected level, carried
/// into [`crate::ai::virtual_::VirtualRouteRequest`] exactly as upstream does.
///
/// Returns `Ok(None)` when `model` is not virtual or its `(provider, id)` pair
/// is no longer registered; `Err` when the static route is registered but its
/// physical model no longer exists (the session logs and leaves the request
/// unrouted, preserving the pre-existing failure surface).
pub(crate) async fn resolve_virtual_request(
    registry: &ProviderRegistry,
    model: &Model,
    context: &mut Arc<crate::ai::types::Context>,
    thinking_level: Option<&str>,
) -> Result<Option<(crate::ai::virtual_::VirtualModelRegistration, Model)>, String> {
    if !crate::ai::virtual_::is_virtual_model(model) {
        return Ok(None);
    }
    let Some((registration, static_physical)) = route_virtual_model(registry, model)? else {
        return Ok(None);
    };
    let mut physical = match registry
        .virtual_model_registry()
        .route_callback(&model.provider, &model.id)
    {
        Some(callback) => {
            let route_request = crate::ai::virtual_::VirtualRouteRequest {
                model: serde_json::to_value(model).unwrap_or(JsonValue::Null),
                thinking_level: thinking_level.map(str::to_owned),
                messages: serde_json::to_value(&context.messages).unwrap_or(JsonValue::Null),
            };
            match callback(route_request).await {
                Ok(decision) => registry
                    .get_physical_model(&decision.physical.provider, &decision.physical.id)
                    .unwrap_or_else(|| {
                        crate::ext_diag!(
                            "[ext] virtual model {} route callback target {}/{} is not a physical model; using the static route",
                            registration.key(),
                            decision.physical.provider,
                            decision.physical.id
                        );
                        static_physical.clone()
                    }),
                Err(error) => {
                    crate::ext_diag!(
                        "[ext] virtual model {} route callback failed; using the static route: {error}",
                        registration.key()
                    );
                    static_physical.clone()
                }
            }
        }
        None => static_physical,
    };
    if let Some(extra) = &registration.system_prompt {
        // The registration's prompt is prepended clone-on-write: the shared
        // context is cloned only when the routing actually rewrites the
        // prompt.
        let mut routed_context = Arc::clone(context);
        let shared = Arc::make_mut(&mut routed_context);
        shared.system_prompt = Some(match shared.system_prompt.take() {
            Some(existing) => format!("{extra}\n\n{existing}"),
            None => extra.clone(),
        });
        *context = routed_context;
    }
    apply_virtual_model_overrides(&registration, &mut physical);
    Ok(Some((registration, physical)))
}

/// Apply the virtual registration's `modelOverride` to the routed physical
/// model. This is the shared projection both request paths use: the session's
/// `prepare_request` returns this physical model to the loop, and
/// `ProviderService::complete` streams it — so `maxTokens` (and the context
/// window used for the request clamp) cannot disagree between an AgentSession
/// prompt and an explicit completion of the same virtual model. Caller-level
/// completion options still win over the registration default in `complete`.
fn apply_virtual_model_overrides(
    registration: &crate::ai::virtual_::VirtualModelRegistration,
    physical: &mut Model,
) {
    let Some(overrides) = registration.model_override else {
        return;
    };
    if let Some(max_tokens) = overrides.max_tokens {
        physical.max_tokens = max_tokens;
    }
    if let Some(context_window) = overrides.context_window {
        physical.context_window = context_window;
    }
}
/// Restore the virtual models persisted on the session branch
/// (`pi.virtual-model-state` custom entries) into the provider registry's
/// overlay so a resumed session keeps its virtual selection routable.
///
/// A session opened TAIL-FIRST (`open_paged`) holds only its newest window
/// resident; the skipped prefix is STREAMED here (one entry at a time, in file
/// order) so a registration persisted before the window still restores —
/// without materializing the transcript at construction time, which is exactly
/// what the paged open exists to avoid.
pub(super) fn restore_session_virtual_models(
    registry: &ProviderRegistry,
    session: &Option<SessionManager>,
) {
    if let Some(session) = session {
        restore_virtual_models_from(registry, session);
    }
}

/// [`restore_session_virtual_models`] for one BORROWED manager: the shared body,
/// also called by hosts that must restore a resumed session's registrations
/// BEFORE resolving the recorded selection (the SDK builder, `attach_session`,
/// and the server's lazy rebuild) — the constructor-time restore runs after
/// those hosts have already looked the model up.
///
/// Returns the `(provider, id)` keys the branch DECLARES via its
/// `pi.virtual-model-state` entries, restored or not: a host that resolves the
/// recorded `projection.model` to nothing can then tell an unresolvable VIRTUAL
/// selection (a missing extension registration — surface an error) from a stale
/// physical id (keep the default, as before).
#[cfg_attr(not(any(feature = "sdk", feature = "server")), allow(dead_code))]
pub(crate) fn restore_virtual_models_from(
    registry: &ProviderRegistry,
    session: &SessionManager,
) -> Vec<(String, String)> {
    let mut declared = Vec::new();
    if session.is_paged() {
        // `for_each_prefix_entry` skips entries that fail to parse, like the
        // full loader's `unparsed_raw` lines do.
        let _ = session.for_each_prefix_entry(|entry| {
            if let Some(key) = restore_virtual_model_entry(registry, &entry) {
                declared.push(key);
            }
        });
    }
    // Borrowed walk: `entries()` cloned every resident entry (messages/tool
    // results included). The header cannot carry a registration, so skipping
    // it leaves the same restore sequence without the clones.
    for entry in session.resident_entries() {
        if entry.is_header() {
            continue;
        }
        if let Some(key) = restore_virtual_model_entry(registry, entry.as_ref()) {
            declared.push(key);
        }
    }
    declared
}

/// Restore one entry's virtual-model registration, when it is one (the shared
/// body of the prefix stream and the resident walk). Returns the `(provider,
/// id)` key the entry declares, even when the registration itself is rejected.
fn restore_virtual_model_entry(
    registry: &ProviderRegistry,
    entry: &crate::session_manager::FileEntry,
) -> Option<(String, String)> {
    let crate::session_manager::FileEntry::Custom(custom) = entry else {
        return None;
    };
    if custom.custom_type != crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY {
        return None;
    }
    // The persisted shape is `{provider, modelId, state}`; fall back to the
    // definition's own fields so a hand-written or partially malformed entry
    // still names the selection it intended.
    let declared = custom.data.as_ref().and_then(|data| {
        let state = data.get("state");
        let provider = data
            .get("provider")
            .and_then(JsonValue::as_str)
            .or_else(|| state?.get("provider").and_then(JsonValue::as_str))?;
        let id = data
            .get("modelId")
            .and_then(JsonValue::as_str)
            .or_else(|| state?.get("id").and_then(JsonValue::as_str))?;
        Some((provider.to_owned(), id.to_owned()))
    });
    let definition = custom
        .data
        .as_ref()
        .and_then(|data| data.get("state"))
        .cloned()
        .unwrap_or(JsonValue::Null);
    match crate::ai::virtual_::VirtualModelRegistration::from_wire(&definition, "session") {
        Ok(registration) => {
            if let Err(error) = registry.register_virtual_model(registration) {
                crate::ext_diag!("[ext] virtual model restore failed: {error}");
            }
        }
        Err(error) => {
            crate::ext_diag!(
                "[ext] ignoring malformed {entry_type} entry: {error}",
                entry_type = crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY
            )
        }
    }
    declared
}
/// Parse the `models` array of a `registerProvider` config into pir `Model`s.
/// Missing optional fields fall back to pir defaults (zero cost, text input).
fn parse_provider_models(config: &serde_json::Value) -> Result<Vec<Model>, String> {
    let Some(models) = config.get("models").and_then(serde_json::Value::as_array) else {
        return Ok(Vec::new());
    };
    let base_url = config
        .get("baseUrl")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let provider_api = config.get("api").and_then(serde_json::Value::as_str);
    let mut parsed = Vec::with_capacity(models.len());
    for model in models {
        let Some(id) = model.get("id").and_then(serde_json::Value::as_str) else {
            return Err("model entry is missing `id`".to_string());
        };
        let name = model
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id)
            .to_string();
        let api = model
            .get("api")
            .and_then(serde_json::Value::as_str)
            .or(provider_api)
            .unwrap_or("openai-completions")
            .to_string();
        let reasoning = model
            .get("reasoning")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let input: Vec<String> = model
            .get("input")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_else(|| vec!["text".to_string()]);
        let context_window = model
            .get("contextWindow")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(128_000);
        let max_tokens = model
            .get("maxTokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(4_096);
        let base_url = model
            .get("baseUrl")
            .and_then(serde_json::Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| base_url.clone());
        if base_url.is_empty() {
            return Err(format!("model `{id}` has no baseUrl"));
        }
        // Upstream `ProviderModelConfig.type` (provider-composer.ts:54-77):
        // omitted means a chat model; `"image"` / `"classifier"` entries are
        // image-generation / structured-classifier models. The kind decides
        // which catalog operations (stream / generateImages / classify) can
        // reach the model.
        let model_type = match model.get("type").and_then(serde_json::Value::as_str) {
            Some("image") => crate::ai::types::ModelKind::Image,
            Some("classifier") => crate::ai::types::ModelKind::Classifier,
            _ => crate::ai::types::ModelKind::Chat,
        };
        parsed.push(Model {
            id: id.to_string(),
            name,
            api,
            // Provider id is stamped by DynamicProviderConfig::normalized_models.
            provider: String::new(),
            base_url,
            reasoning,
            thinking_level_map: parse_thinking_level_map(model),
            input,
            cost: parse_model_cost(model),
            context_window,
            max_tokens,
            sampling_params: None,
            headers: parse_model_headers(model),
            thinking_token_budget_field: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params_by_thinking_level: None,
            model_type: Some(model_type),
            compat: None,
        });
    }
    Ok(parsed)
}

/// Build the runtime `DynamicProviderConfig` for
/// [`ProviderService::register_provider`](crate::plugins::services::ProviderService::register_provider).
///
/// This is the immediate-registration subset of a `pi.registerProvider`
/// config: `models`, `baseUrl`, `api`, `apiKey`, provider/model `headers` and
/// `authHeader`. Callback-driven fields (`streamSimple`, `refreshModels`,
/// `oauth`) are rejected with a clear message: a native plugin that needs
/// them registers a typed [`crate::ai::provider::Provider`] implementation
/// instead, and a declarative Rust definition still gets the full sync pass.
/// A model-less `baseUrl` definition overrides the existing provider models,
/// matching `sync_plugin_providers`' baseUrl-only path.
pub(crate) fn runtime_dynamic_provider_config(
    registry: &ProviderRegistry,
    id: &str,
    config: &JsonValue,
) -> Result<DynamicProviderConfig, String> {
    for field in ["streamSimple", "refreshModels"] {
        if config.get(field).and_then(JsonValue::as_bool) == Some(true) {
            return Err(format!(
                "provider `{id}`: runtime `{field}` requires a typed Provider or the plugin-registry sync"
            ));
        }
    }
    if config
        .get("oauth")
        .and_then(|oauth| oauth.get("login"))
        .and_then(JsonValue::as_bool)
        == Some(true)
    {
        return Err(format!(
            "provider `{id}`: runtime `oauth` callbacks require a typed Provider"
        ));
    }

    let provider_headers = apply_provider_headers(config);
    let auth_header = config
        .get("authHeader")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let resolved_key = config
        .get("apiKey")
        .and_then(JsonValue::as_str)
        .map(interpolate_api_key)
        .filter(|key| !key.is_empty());
    let mut models = parse_provider_models(config)?;
    let base_url = config
        .get("baseUrl")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    if models.is_empty() {
        if base_url.is_empty() {
            return Err(format!(
                "provider `{id}`: runtime registration needs `models` or a `baseUrl` override"
            ));
        }
        let existing = registry.get_provider_models(id);
        if existing.is_empty() {
            return Err(format!(
                "provider `{id}`: baseUrl-only override but the provider has no known models"
            ));
        }
        models = existing
            .into_iter()
            .map(|mut model| {
                model.base_url = base_url.clone();
                model
            })
            .collect();
    }
    apply_configured_headers(
        &mut models,
        provider_headers.as_ref(),
        auth_header,
        resolved_key.as_deref(),
    );
    Ok(DynamicProviderConfig {
        base_url,
        api_key: resolved_key,
        api: config
            .get("api")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string(),
        models,
        compat: None,
        refresh: None,
    })
}

/// `thinkingLevelMap` (upstream `Model.thinkingLevelMap`): `{level: wireValue}`
/// with `null` disabling a level.
fn parse_thinking_level_map(
    model: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, Option<String>>> {
    let map = model.get("thinkingLevelMap")?.as_object()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (level, value) in map {
        parsed.insert(level.clone(), value.as_str().map(String::from));
    }
    Some(parsed)
}

/// Per-model `cost` (upstream `{input, output, cacheRead, cacheWrite}` plus
/// optional request-wide `tiers`).
fn parse_model_cost(model: &serde_json::Value) -> crate::ai::types::ModelCost {
    let rates = |value: Option<&serde_json::Value>| crate::ai::types::ModelCostRates {
        input: value
            .and_then(|cost| cost.get("input"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
        output: value
            .and_then(|cost| cost.get("output"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
        cache_read: value
            .and_then(|cost| cost.get("cacheRead"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
        cache_write: value
            .and_then(|cost| cost.get("cacheWrite"))
            .and_then(JsonValue::as_f64)
            .unwrap_or(0.0),
    };
    let cost = model.get("cost");
    crate::ai::types::ModelCost {
        rates: rates(cost),
        tiers: cost
            .and_then(|cost| cost.get("tiers"))
            .and_then(JsonValue::as_array)
            .map(|tiers| {
                tiers
                    .iter()
                    .map(|tier| crate::ai::types::ModelCostTier {
                        input_tokens_above: tier
                            .get("inputTokensAbove")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0),
                        rates: rates(Some(tier)),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Per-model request `headers` (upstream `Model.headers`).
fn parse_model_headers(
    model: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, String>> {
    let headers = model.get("headers")?.as_object()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (name, value) in headers {
        if let Some(value) = value.as_str() {
            parsed.insert(name.clone(), value.to_owned());
        }
    }
    Some(parsed)
}

/// Merge the provider-level `headers` and `authHeader` credential into every
/// model (provider headers first, model headers win; `authHeader` writes the
/// final `Authorization`). Re-run after any catalog replacement — a refresh
/// returns bare model configs, so without this pass every refreshed model
/// would lose the configured request headers.
fn apply_configured_headers(
    models: &mut [Model],
    provider_headers: Option<&std::collections::BTreeMap<String, String>>,
    auth_header: bool,
    resolved_key: Option<&str>,
) {
    if provider_headers.is_none() && !auth_header {
        return;
    }
    for model in models {
        let mut merged = provider_headers.cloned().unwrap_or_default();
        for (name, value) in model.headers.iter().flatten() {
            merged.insert(name.clone(), value.clone());
        }
        if auth_header {
            if let Some(key) = resolved_key {
                merged.insert("Authorization".to_string(), format!("Bearer {key}"));
            }
        }
        model.headers = (!merged.is_empty()).then_some(merged);
    }
}

/// The provider's persisted catalog for this session: the registry's injected
/// store first (it is authoritative for the live process), then — when that
/// store is memory-only, as `ProviderRegistry::default()` builds it — this
/// session's agent-dir `models-store.json` so a host that never injected a
/// [`crate::ai::providers::models_store::FileModelsStore`] still restores
/// across restarts.
fn read_provider_catalog(
    session: &AgentSession,
    provider_id: &str,
) -> Option<crate::ai::providers::models_store::ModelsStoreEntry> {
    let store = session.registry.models_store();
    match store.read(provider_id) {
        Ok(Some(entry)) => return Some(entry),
        Ok(None) => {}
        Err(error) => {
            crate::ext_diag!(
                "[ext] provider `{provider_id}`: models-store read failed: {}",
                error.user_message()
            );
        }
    }
    if store.is_file_backed() {
        return None;
    }
    crate::ai::providers::models_store::read_models_store_entry(&session.agent_dir, provider_id)
}

/// Apply one `publish({persist})` decision from the JS refresh callback to the
/// registry's [`crate::ai::providers::models_store::ModelsStore`]
/// (`null` deletes, an object upserts). Persisted model objects are stamped
/// with the provider id when they omit it, so the restored catalog is
/// provider-scoped like every catalog the store holds.
///
/// A memory-only registry store (the default) is mirrored into the session's
/// agent-dir `models-store.json`; a file-backed store already is that file and
/// is not written twice.
fn persist_provider_catalog(
    session: &AgentSession,
    provider_id: &str,
    publication: &serde_json::Value,
) {
    let store = session.registry.models_store();
    if publication.is_null() {
        if let Err(error) = store.delete(provider_id) {
            crate::ext_diag!(
                "[ext] provider `{provider_id}`: models-store delete failed: {}",
                error.user_message()
            );
        }
        if !store.is_file_backed() {
            if let Err(error) = crate::ai::providers::models_store::delete_models_store_entry(
                &session.agent_dir,
                provider_id,
            ) {
                crate::ext_diag!(
                    "[ext] provider `{provider_id}`: models-store file delete failed: {error}"
                );
            }
        }
        return;
    }
    let Some(mut entry) =
        crate::ai::providers::models_store::ModelsStoreEntry::from_wire(publication)
    else {
        crate::ext_diag!(
            "[ext] provider `{provider_id}`: publish persist is not an object; ignoring"
        );
        return;
    };
    for model in &mut entry.models {
        if let Some(object) = model.as_object_mut() {
            let missing = object
                .get("provider")
                .and_then(JsonValue::as_str)
                .is_none_or(str::is_empty);
            if missing {
                object.insert(
                    "provider".to_owned(),
                    JsonValue::String(provider_id.to_owned()),
                );
            }
        }
    }
    if let Err(error) = store.write(provider_id, entry.clone()) {
        crate::ext_diag!(
            "[ext] provider `{provider_id}`: models-store write failed: {}",
            error.user_message()
        );
    }
    if !store.is_file_backed() {
        if let Err(error) = crate::ai::providers::models_store::write_models_store_entry(
            &session.agent_dir,
            provider_id,
            entry,
        ) {
            crate::ext_diag!(
                "[ext] provider `{provider_id}`: models-store file write failed: {error}"
            );
        }
    }
}

/// Decode a persisted catalog into models, falling back to the provider config
/// for every field the store entry omitted. `None` means the entry supplies no
/// usable models.
fn restore_models_from_store(
    config: &serde_json::Value,
    provider_id: &str,
    entry: &crate::ai::providers::models_store::ModelsStoreEntry,
) -> Option<Vec<Model>> {
    if entry.models.is_empty() {
        return None;
    }
    let stored_config = serde_json::json!({
        "baseUrl": config.get("baseUrl").cloned().unwrap_or(JsonValue::Null),
        "api": config.get("api").cloned().unwrap_or(JsonValue::Null),
        "models": entry.models,
    });
    match parse_provider_models(&stored_config) {
        Ok(models) if !models.is_empty() => Some(models),
        Ok(_) => None,
        Err(error) => {
            crate::ext_diag!("[ext] provider `{provider_id}`: persisted catalog ignored: {error}");
            None
        }
    }
}

/// Provider-level request `headers` (upstream `ProviderConfig.headers`):
/// `$VAR`/`${VAR}` references interpolate from the process environment (same
/// expansion as the extension API key; upstream `resolveHeadersOrThrow`).
fn apply_provider_headers(
    config: &serde_json::Value,
) -> Option<std::collections::BTreeMap<String, String>> {
    let headers = config.get("headers")?.as_object()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (name, value) in headers {
        if let Some(value) = value.as_str() {
            parsed.insert(name.clone(), interpolate_api_key(value));
        }
    }
    Some(parsed)
}

/// Expand `${VAR}` / `$VAR` references in an extension-provided API key from
/// the process environment (upstream interpolates credentials).
fn interpolate_api_key(value: &str) -> String {
    if !value.contains('$') {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch != '$' {
            out.push(ch);
            index += 1;
            continue;
        }
        // ${VAR} form first.
        if index + 1 < chars.len() && chars[index + 1] == '{' {
            if let Some(close) = chars[index + 2..].iter().position(|c| *c == '}') {
                let name: String = chars[index + 2..index + 2 + close].iter().collect();
                if !name.is_empty() {
                    if let Ok(value) = std::env::var(&name) {
                        out.push_str(&value);
                    }
                    index += close + 3;
                    continue;
                }
            }
        }
        // $VAR form.
        let mut name = String::new();
        let mut cursor = index + 1;
        while cursor < chars.len() && (chars[cursor].is_alphanumeric() || chars[cursor] == '_') {
            name.push(chars[cursor]);
            cursor += 1;
        }
        if !name.is_empty() {
            if let Ok(value) = std::env::var(&name) {
                out.push_str(&value);
            }
            index = cursor;
            continue;
        }
        out.push('$');
        index += 1;
    }
    out
}
/// Serialize the session-only model scope for the runtime's `ctx.scopedModels`
/// (upstream `ScopedModel[]`: `{model, thinkingLevel?}` entries carrying the
/// FULL serialized model object; `thinkingLevel` omitted when unset).
pub fn serialize_scoped_models(models: &[crate::ai::providers::ScopedModel]) -> JsonValue {
    JsonValue::Array(
        models
            .iter()
            .map(|scoped| {
                let mut entry = serde_json::Map::new();
                entry.insert(
                    "model".to_owned(),
                    serde_json::to_value(&scoped.model).unwrap_or(JsonValue::Null),
                );
                if let Some(level) = &scoped.thinking_level {
                    entry.insert(
                        "thinkingLevel".to_owned(),
                        JsonValue::String(level.wire_key().to_owned()),
                    );
                }
                JsonValue::Object(entry)
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Provider-scoped request credentials
// ---------------------------------------------------------------------------

/// Capacity bound of [`MODELS_JSON_KEY_CACHE`]: the most recent agent
/// directories whose `models.json` provider keys were resolved (least recently
/// used evicted first), so a process hosting agents in many directories cannot
/// grow the cache without limit.
const MODELS_JSON_KEY_CACHE_CAPACITY: usize = 4;

/// One cached `models.json` provider-key map: the agent directory it was
/// resolved for, that file's stamp when it was read, and the resolved keys.
struct ModelsJsonKeyCache {
    agent_dir: PathBuf,
    /// Length+mtime of `<agent_dir>/models.json` at read time (`None` = the
    /// file did not exist).
    stamp: Option<crate::config::settings::FileStamp>,
    keys: std::collections::BTreeMap<String, String>,
}

fn models_json_key_cache() -> &'static Mutex<Vec<ModelsJsonKeyCache>> {
    static CACHE: std::sync::OnceLock<Mutex<Vec<ModelsJsonKeyCache>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// The configured `models.json` API key of every provider, resolved against
/// `agent_dir`.
///
/// Same composition the shared loader applies when it selects a model
/// (`load_pi_config_from`) and the SDK applies for its own agent directory:
/// [`crate::ai::providers::parse_pi_config`] for the JSONC file plus
/// [`crate::ai::providers::config_value::resolve_config_value_in`] for each
/// provider's `apiKey` template (`$ENV` / `${ENV}` / literal / cached
/// `!command`). The session resolves against ITS OWN agent directory (D1), not
/// the live process environment.
///
/// **Cost.** A hit is one `metadata` call (the file stamp) plus a map clone. A
/// miss re-reads and re-parses once; `resolve_config_value_in` keys its own
/// `!command` results on the same directory, so a command value is never
/// re-run on a hot path.
///
/// **Bound.** [`MODELS_JSON_KEY_CACHE_CAPACITY`] entries, least recently used
/// evicted first. An ABSENT file caches an empty map under its `None` stamp, so
/// the common "this agent dir has no models.json" case costs one `metadata`
/// call per directory; a file that appears later changes the stamp and is read
/// on the next request.
fn models_json_api_keys(agent_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let path = agent_dir.join("models.json");
    let stamp = crate::config::settings::file_stamp(&path);
    {
        let mut cache = models_json_key_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(position) = cache
            .iter()
            .position(|entry| entry.agent_dir.as_path() == agent_dir && entry.stamp == stamp)
        {
            // Move to the back: most recently used.
            let entry = cache.remove(position);
            let keys = entry.keys.clone();
            cache.push(entry);
            return keys;
        }
    }
    let mut keys = std::collections::BTreeMap::new();
    if let Ok(contents) = std::fs::read_to_string(&path) {
        if let Ok(config) = crate::ai::providers::parse_pi_config(&contents) {
            for (provider_id, provider_config) in &config.providers {
                let key = provider_config.api_key.as_deref().and_then(|template| {
                    crate::ai::providers::config_value::resolve_config_value_in(
                        agent_dir, template, None,
                    )
                });
                if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
                    keys.insert(provider_id.clone(), key);
                }
            }
        }
    }
    {
        let mut cache = models_json_key_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|entry| entry.agent_dir.as_path() != agent_dir);
        while cache.len() >= MODELS_JSON_KEY_CACHE_CAPACITY {
            cache.remove(0);
        }
        cache.push(ModelsJsonKeyCache {
            agent_dir: agent_dir.to_path_buf(),
            stamp,
            keys: keys.clone(),
        });
    }
    keys
}

impl AgentSession {
    /// The API key to send with a request that runs under `model`.
    ///
    /// Provider-scoped by construction, in precedence order:
    ///
    /// 1. the explicit key the session was built with
    ///    (`AgentSessionConfig::api_key`) — but only while it belongs to this
    ///    provider, so `/model` across providers can never send it to another
    ///    endpoint;
    /// 2. a key an extension registered for this provider
    ///    (`pi.registerProvider({apiKey})`, `extension_api_keys`: the
    ///    map's key IS the provider scope);
    /// 3. the provider's own `models.json` `apiKey`, resolved from this
    ///    session's agent directory (`models_json_api_keys`, stamp-cached).
    ///
    /// `None` means "this session holds no credential for the provider": the
    /// request then carries no key, so the provider's own resolution (ambient
    /// env vars, model headers, the registry's stored credential) applies and
    /// an unconfigured provider reports its "not configured / login required"
    /// outcome instead of receiving another provider's key and answering 401.
    ///
    /// A virtual selection routes every request to a physical model, so the
    /// credential resolved is the PHYSICAL provider's — the one that actually
    /// receives the request.
    pub(super) fn request_api_key_for(&self, model: &Model) -> Option<String> {
        let routed_provider = crate::ai::virtual_::is_virtual_model(model)
            .then(|| {
                self.registry
                    .virtual_model_registry()
                    .get(&model.provider, &model.id)
                    .map(|registration| registration.physical.provider)
            })
            .flatten();
        let provider = routed_provider
            .as_deref()
            .unwrap_or(model.provider.as_str());
        if let Some(api_key) = &self.api_key {
            if api_key.provider == provider {
                return Some(api_key.key.clone());
            }
        }
        if let Some(key) = self
            .extension_api_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
        {
            if !key.trim().is_empty() {
                return Some(key.clone());
            }
        }
        models_json_api_keys(&self.agent_dir).get(provider).cloned()
    }

    /// The session's EXPLICIT provider-scoped credential (the key it was
    /// built with), as the child-run environment
    /// ([`crate::agent::tool_execution::ToolSessionEnv::credential`]) carries
    /// it.
    ///
    /// Environment/models.json credentials are deliberately NOT included: a
    /// child built with the propagated
    /// [`crate::agent::tool_execution::ToolSessionEnv::agent_dir`] resolves
    /// those itself through the same chain this session uses.
    pub(crate) fn explicit_tool_credential(
        &self,
    ) -> Option<crate::agent::tool_execution::ToolCredential> {
        self.api_key.as_ref().map(|key| {
            crate::agent::tool_execution::ToolCredential::new(key.provider.clone(), key.key.clone())
        })
    }
}
