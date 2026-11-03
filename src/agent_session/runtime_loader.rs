use super::*;

/// Load the merged settings for `cwd` (global scope + trusted project scope),
/// the same `SettingsManager` the CLI builds.
pub(super) fn load_settings_for(cwd: &Path) -> SettingsManager {
    load_settings_for_in(&crate::coding_agent::context::agent_dir(), cwd)
}

/// [`load_settings_for`] against an EXPLICIT agent directory (D1): the session
/// host passes its own build-time directory so a live session keeps reading
/// the settings it started with when `PIR_AGENT_DIR` changes underneath it.
/// `agent_dir` is the caller's already-resolved value; this function never
/// touches the process environment for it.
pub(super) fn load_settings_for_in(agent_dir: &Path, cwd: &Path) -> SettingsManager {
    // The same resolution order every other trust consumer uses: operator
    // override, the extension `project_trust` decision (recorded at load
    // time), then the persisted trust store / `defaultProjectTrust` policy.
    let trusted = crate::extensions::project_trust::resolve_project_trust(cwd, agent_dir);
    SettingsManager::load(agent_dir, &cwd.join(".pi"), cwd, trusted)
}

// ---------------------------------------------------------------------------
// Extension runtime plumbing (shared by the print/RPC/TUI hosts)
// ---------------------------------------------------------------------------

/// A validated extension runtime parked for handoff by the CLI's strict
/// pre-validation ([`load_extension_runtime_strict`] via `main.rs`): the very
/// runtime that proved the extension set loads is reused by the mode host so
/// discovery, swc transpile, top-level evaluation and flag application run
/// once instead of twice. (`project_trust` belongs to the separate disposable
/// pre-trust bootstrap pass; neither of these loads dispatches it.)
struct ValidatedRuntime {
    paths: Vec<PathBuf>,
    config: ExtensionConfig,
    runtime: ExtensionHandle,
}

static VALIDATED_RUNTIME: std::sync::Mutex<Option<ValidatedRuntime>> = std::sync::Mutex::new(None);

/// Park the validated runtime for the next [`load_extension_runtime`] call
/// with the same paths and a compatible config. A host that never consumes it
/// (or one loading under a different mode/UI config) simply drops it and keeps
/// its own load path.
pub fn stash_validated_extension_runtime(
    paths: &[PathBuf],
    config: ExtensionConfig,
    runtime: ExtensionHandle,
) {
    let mut slot = VALIDATED_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = slot.replace(ValidatedRuntime {
        paths: paths.to_vec(),
        config,
        runtime,
    });
    if let Some(previous) = previous {
        // A replaced parked runtime also never became the live session; its
        // teardown must stay silent for the same reason as a config mismatch.
        previous.runtime.suppress_session_shutdown_on_drop();
    }
}

/// Whether a parked runtime's construction config can serve a host that asks
/// for `requested`. The resolved `model` is the one deliberate exclusion: the
/// validation/pre-trust load does not know it yet, so it is pushed through the
/// session snapshot at handoff, matching what a fresh host load exposes.
pub fn extension_config_matches_for_handoff(
    parked: &ExtensionConfig,
    requested: &ExtensionConfig,
) -> bool {
    parked.mode == requested.mode
        && parked.has_ui == requested.has_ui
        && parked.cwd == requested.cwd
        && parked.session_id == requested.session_id
        && parked.system_prompt == requested.system_prompt
        && parked.is_idle == requested.is_idle
        && parked.limits == requested.limits
}

/// Adopt the CLI's disposable pre-trust bootstrap runtime as the LIVE host
/// runtime.
///
/// Upstream `ResourceLoader.reload` passes `preTrustExtensions.runtime` into
/// `loadFinalExtensionSet`, so the instance that observed `project_trust`
/// continues into `session_start` and the session; its module/closure state
/// and queued `ctx.ui` notifications are not thrown away. pir cannot append
/// more paths to an already-loaded runtime, so the CLI adopts only when the
/// final extension plan is exactly the bootstrap plan (same paths and
/// construction config). Otherwise the bootstrap instance is disposed and the
/// strict-validation runtime serves the host.
///
/// Re-enables the normal teardown event: this handle now represents the real
/// session, unlike a disposable pass that is dropped after recording trust.
pub fn adopt_pre_trust_extension_runtime(runtime: ExtensionHandle) -> ExtensionHandle {
    runtime.allow_session_shutdown_on_drop();
    runtime
}

/// Dispose an extension runtime that will never become the live session
/// without letting its mailbox-close tail masquerade as a real
/// `session_shutdown` (the duplicate strict-validation runtime when the
/// pre-trust runtime is adopted).
pub fn discard_extension_runtime(runtime: ExtensionHandle) {
    runtime.suppress_session_shutdown_on_drop();
    drop(runtime);
}

/// Take the parked validation runtime when `paths` match and every config
/// field the runtime is built from matches the caller's request. `limits` is
/// one of those construction inputs: `ExtensionRuntime::new_with_state`
/// resolves `config.limits` into `RuntimeState::limits` and applies it to the
/// QuickJS engine, so a parked runtime built under different limits must not
/// be reused. `None` (handoff absent or incompatible) keeps the caller's own
/// load path.
fn take_validated_extension_runtime(
    paths: &[PathBuf],
    config: &ExtensionConfig,
) -> Option<ExtensionHandle> {
    let mut slot = VALIDATED_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let validated = slot.take()?;
    let matches =
        validated.paths == paths && extension_config_matches_for_handoff(&validated.config, config);
    if !matches {
        // The parked runtime never became the live session (different
        // paths/config), so its teardown must not dispatch a startup-time
        // `session_shutdown` to its JS handlers. Before this, a `--mode json`
        // host (whose final config used `"json"` while the validation config
        // said `"print"`) dropped the strict runtime here and the extension
        // observed a fake shutdown before `session_start`.
        validated.runtime.suppress_session_shutdown_on_drop();
        return None;
    }
    if let Some(model) = &config.model {
        // Parity with a fresh host load: `ctx.model` falls back to the config
        // model there, and to the session snapshot here.
        validated
            .runtime
            .update_session_context(SessionContextSnapshot {
                model: serde_json::Value::String(model.clone()),
                project_trusted: true,
                ..SessionContextSnapshot::default()
            });
    }
    Some(validated.runtime)
}

// ---------------------------------------------------------------------------
// Persistent negative cache for extension load failures
// ---------------------------------------------------------------------------

/// One persisted load failure: the entry file's content fingerprint (mtime +
/// size) at the time the failure was recorded, plus a one-line reason summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CachedExtensionFailure {
    mtime_secs: u64,
    mtime_nanos: u32,
    size: u64,
    reason: String,
}

/// On-disk layout of `extension-fail-cache.json` in the agent dir
/// (`~/.pi/agent` by default, same directory family as the other agent-side
/// state). Keys are extension entry paths with backslashes normalized to `/`
/// so a path round-trips across Windows path conventions.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ExtensionFailureCacheFile {
    version: u32,
    entries: std::collections::BTreeMap<String, CachedExtensionFailure>,
}

const EXTENSION_FAILURE_CACHE_VERSION: u32 = 1;
/// Cap on a stored reason so one chatty failure cannot bloat the cache file.
const EXTENSION_FAILURE_REASON_MAX_CHARS: usize = 300;

/// Path of the persistent extension-failure cache (`PI_EXT_RELOAD` is the
/// manual bypass; the file lives beside the other `~/.pi/agent` state).
fn extension_failure_cache_path() -> PathBuf {
    crate::coding_agent::context::agent_dir().join("extension-fail-cache.json")
}

/// Cache key for an extension entry path: backslashes normalized to `/` for
/// stable round-tripping (discovery paths mix separators on Windows).
fn extension_failure_cache_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Content fingerprint of an extension entry file: mtime + size. A hash of
/// the contents would be stronger, but the failure class this cache exists
/// for (missing native bindings, architecture boundaries) is deterministic in
/// the file's identity, and mtime+size costs two stat calls instead of a full
/// read per candidate on every startup.
fn extension_fingerprint(path: &Path) -> Option<(u64, u32, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let since_epoch = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Some((
        since_epoch.as_secs(),
        since_epoch.subsec_nanos(),
        metadata.len(),
    ))
}

/// Read the failure cache; any I/O or parse problem degrades to an empty
/// cache (the cost is one repeated failed load, never a broken startup).
fn read_extension_failure_cache() -> ExtensionFailureCacheFile {
    std::fs::read_to_string(extension_failure_cache_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist the failure cache, creating the agent dir if needed. A write
/// failure is silently ignored — the cache is an optimization, not state.
fn write_extension_failure_cache(file: &ExtensionFailureCacheFile) {
    let path = extension_failure_cache_path();
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            crate::ext_diag!("[ext] cannot create {}: {error}", parent.display());
            return;
        }
    }
    match serde_json::to_string_pretty(file) {
        Ok(text) => {
            if let Err(error) = std::fs::write(&path, text) {
                crate::ext_diag!("[ext] cannot write {}: {error}", path.display());
            }
        }
        Err(error) => crate::ext_diag!("[ext] cannot serialize the failure cache: {error}"),
    }
}

/// One-line reason summary for the cache: the first line of the error,
/// truncated on a char boundary. A first line that merely introduces a list
/// (trailing `:`) loses the colon so the summary reads as a complete phrase.
fn summarize_extension_failure(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or("unknown error");
    let first_line = first_line.trim_end();
    let first_line = first_line.strip_suffix(':').unwrap_or(first_line);
    let summary: String = first_line
        .chars()
        .take(EXTENSION_FAILURE_REASON_MAX_CHARS)
        .collect();
    if summary.is_empty() {
        "unknown error".to_string()
    } else {
        summary
    }
}

/// Whether the negative cache may skip paths for this load: only the one-shot
/// headless hosts (`print` covers both text print and `--mode json`, which
/// validate under their host's exact extension runtime mode). Interactive/
/// TUI/RPC hosts never skip — an interactive session must be able to retry a
/// fixed extension, and a long-lived RPC host should not bake a startup-time
/// failure into its whole lifetime. `PI_EXT_RELOAD=1` forces every host
/// through a real load.
fn extension_negative_cache_enabled(host_mode: &str) -> bool {
    matches!(host_mode, "print" | "json")
        && !std::env::var_os("PI_EXT_RELOAD").is_some_and(|value| value == "1")
}

/// Split the on-disk load list into (still-to-load, cached-failures): a path
/// whose current fingerprint matches a persisted failure is dropped from the
/// load list with its recorded reason.
pub(crate) fn partition_cached_extension_failures(
    file_paths: Vec<PathBuf>,
    host_mode: &str,
) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    if !extension_negative_cache_enabled(host_mode) {
        return (file_paths, Vec::new());
    }
    let cache = read_extension_failure_cache();
    if cache.entries.is_empty() {
        return (file_paths, Vec::new());
    }
    let mut load = Vec::with_capacity(file_paths.len());
    let mut skipped = Vec::new();
    for path in file_paths {
        let hit = cache
            .entries
            .get(&extension_failure_cache_key(&path))
            .and_then(|entry| {
                extension_fingerprint(&path).map(|fp| {
                    (
                        entry,
                        fp.0 == entry.mtime_secs && fp.1 == entry.mtime_nanos && fp.2 == entry.size,
                    )
                })
            })
            .filter(|(_, matches)| *matches)
            .map(|(entry, _)| entry.reason.clone());
        match hit {
            Some(reason) => skipped.push((path, reason)),
            None => load.push(path),
        }
    }
    (load, skipped)
}

/// Fold one load outcome into the cache: failures are recorded under their
/// entry file's current fingerprint; successfully loaded paths drop any stale
/// entry (self-heal — e.g. the user installed the missing native bindings and
/// the entry file's mtime never moved). One write at most per load.
fn update_extension_failure_cache(result: &crate::extensions::LoadResult) {
    if result.loaded.is_empty() && result.errors.is_empty() {
        return;
    }
    let mut file = read_extension_failure_cache();
    let mut dirty = false;
    for extension in &result.loaded {
        if file
            .entries
            .remove(&extension_failure_cache_key(&extension.path))
            .is_some()
        {
            dirty = true;
        }
    }
    for error in &result.errors {
        let Some((mtime_secs, mtime_nanos, size)) = extension_fingerprint(&error.path) else {
            continue;
        };
        let entry = CachedExtensionFailure {
            mtime_secs,
            mtime_nanos,
            size,
            reason: summarize_extension_failure(&error.error),
        };
        if file.entries.get(&extension_failure_cache_key(&error.path)) != Some(&entry) {
            file.entries
                .insert(extension_failure_cache_key(&error.path), entry);
            dirty = true;
        }
    }
    if dirty {
        file.version = EXTENSION_FAILURE_CACHE_VERSION;
        write_extension_failure_cache(&file);
    }
}

/// Serialized model-catalog snapshot pushed into an extension runtime BEFORE
/// extensions are loaded.
///
/// Extension factories run during [`ExtensionHandle::load`] and may resolve a
/// model synchronously (`modelRuntime.getModels()`, `ctx.modelRegistry`), so
/// the catalog has to exist on the runtime before the load command reaches
/// the actor. The session host pushes the same snapshot through
/// `AgentSession::push_session_context` — but only after the child session is
/// built, which is too late for a factory-time lookup. This helper is the
/// single source of the wire shape used by both paths:
/// `{models: [serialized Model...], providers: [{id, name}...]}`.
///
/// [`ProviderRegistry::get_models`] already merges dynamically registered
/// (extension-owned) providers and virtual-model overlays, so a catalog built
/// from the parent registry includes them.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ModelCatalog {
    models: JsonValue,
    providers: JsonValue,
    /// `None` keeps the runtime's permissive-empty convention (the host
    /// cannot check, so `hasConfiguredAuth` answers `true`).
    authenticated_providers: Option<Vec<String>>,
}

impl ModelCatalog {
    /// Serialize the registry's current catalog (models + providers).
    pub fn from_registry(registry: &ProviderRegistry) -> Self {
        Self::from_models(registry.get_models(), registry)
    }

    fn from_models(models: Vec<Model>, registry: &ProviderRegistry) -> Self {
        let models: Vec<JsonValue> = models
            .iter()
            .filter_map(|model| serde_json::to_value(model).ok())
            .collect();
        let providers: Vec<JsonValue> = registry
            .providers()
            .iter()
            .map(|provider| serde_json::json!({ "id": provider.id(), "name": provider.name() }))
            .collect();
        Self {
            models: JsonValue::Array(models),
            providers: JsonValue::Array(providers),
            authenticated_providers: None,
        }
    }

    /// Serialize the catalog and resolve the provider ids that hold a usable
    /// credential for `cwd` (the same resolution
    /// `AgentSession::authenticated_providers` performs). An unreadable
    /// credential environment leaves the list absent, preserving the
    /// permissive-empty runtime convention.
    pub fn from_registry_with_authenticated_providers(
        registry: &ProviderRegistry,
        cwd: &Path,
    ) -> Self {
        let mut catalog = Self::from_registry(registry);
        if let Ok(environment) = crate::ai::providers::load_pi_environment(Some(cwd)) {
            catalog.authenticated_providers = Some(
                registry
                    .providers()
                    .iter()
                    .filter(|provider| {
                        crate::ai::providers::has_configured_auth(
                            provider.id(),
                            &environment.api_keys,
                        )
                    })
                    .map(|provider| provider.id().to_owned())
                    .collect(),
            );
        }
        catalog
    }

    /// Attach the authenticated-provider list (permissive-empty convention:
    /// an empty list is authoritative "nothing configured", an absent list is
    /// permissive).
    pub fn with_authenticated_providers(mut self, providers: Vec<String>) -> Self {
        self.authenticated_providers = Some(providers);
        self
    }

    /// Push the snapshot into `runtime`. Must run BEFORE
    /// [`ExtensionHandle::load`] for factories that resolve models during
    /// load; harmless (idempotent state write) afterwards too.
    pub fn apply(&self, runtime: &ExtensionHandle) {
        runtime.update_model_registry(self.models.clone(), self.providers.clone());
        if let Some(providers) = &self.authenticated_providers {
            runtime.update_authenticated_providers(providers.clone());
        }
    }

    /// [`Self::apply`] that MOVES the serialized catalog into the runtime
    /// instead of deep-copying it. The per-prompt session push rebuilds the
    /// catalog only when its fingerprint changes and then applies it once, so
    /// the cached copy is not retained: consuming the values removes the
    /// final `Vec<Value>` clone from that path.
    pub(crate) fn apply_owned(self, runtime: &ExtensionHandle) {
        let Self {
            models,
            providers,
            authenticated_providers,
        } = self;
        runtime.update_model_registry(models, providers);
        if let Some(providers) = authenticated_providers {
            runtime.update_authenticated_providers(providers);
        }
    }

    /// The serialized model array (`models` key of the wire snapshot).
    pub fn models(&self) -> &JsonValue {
        &self.models
    }

    /// The serialized provider array (`providers` key of the wire snapshot).
    pub fn providers(&self) -> &JsonValue {
        &self.providers
    }
}

/// Build and load an extension runtime for the given paths.
///
/// The path list follows the unified loader ordering (`src/plugins/loader`):
/// CLI `-e` paths first, then settings/default paths, `builtin:<name>`
/// entries included in order. Builtins resolve through the Rust builtin
/// factory list ([`crate::plugins::builtin`]); unknown names surface the
/// upstream `Unknown built-in extension:` error. File paths load through the
/// QuickJS runtime; builtin names ride the runtime's recorded path plan so
/// the unified plugin host can derive plugin dispatch order.
///
/// The plan is partitioned BEFORE the runtime starts: when no on-disk JS path
/// survives (a builtin-only plan, including one whose only file paths were
/// dropped by the negative cache or the native `tool-repair` precedence), the
/// handle is built with [`ExtensionHandle::start_plan_only`] — same shared
/// state and path plan, but no actor thread and no QuickJS runtime/prelude
/// (the ~10.6 MB builtin-only startup cost).
///
/// Returns `None` when no paths are configured or the runtime cannot start;
/// load-time errors are reported through the diagnostic router and never fail host startup (an
/// extension failing to load is isolated, per the runtime contract).
pub async fn load_extension_runtime(
    paths: &[PathBuf],
    config: ExtensionConfig,
) -> Option<ExtensionHandle> {
    load_extension_runtime_impl(paths, config, None, false).await
}

/// Load the CLI's DISPOSABLE pre-trust extension pass (upstream
/// `ResourceLoader.loadProjectTrustExtensions`): the same loading as
/// [`load_extension_runtime`], but
///
/// - `project_trust` is dispatched to the JS face here, exactly once per
///   face — final host loads ([`load_extension_runtime`],
///   [`load_extension_runtime_with_catalog`], the strict variants) never
///   dispatch it;
/// - the returned runtime is created with
///   [`ExtensionHandle::start_disposable`], so dropping it does NOT emit
///   `session_shutdown` to the extension handlers. That event belongs to the
///   real session runtime, whose teardown emits it exactly once.
///
/// The caller is expected to drop the handle as soon as the recorded trust
/// decision has been read (see [`crate::extensions::project_trust`]).
pub async fn load_extension_runtime_bootstrap(
    paths: &[PathBuf],
    config: ExtensionConfig,
) -> Option<ExtensionHandle> {
    load_extension_runtime_impl(paths, config, None, true).await
}

/// [`load_extension_runtime`] with a model catalog pushed onto the runtime
/// BEFORE `load`, so an extension factory that resolves a model synchronously
/// at load time sees the host's models/providers instead of the empty default.
///
/// Hosts that already hold the provider registry pass
/// `Some(ModelCatalog::from_registry(&registry))`; hosts that do not (or
/// deliberately defer configuration) pass `None` and keep the old behavior.
pub async fn load_extension_runtime_with_catalog(
    paths: &[PathBuf],
    config: ExtensionConfig,
    catalog: Option<&ModelCatalog>,
) -> Option<ExtensionHandle> {
    load_extension_runtime_impl(paths, config, catalog, false).await
}

/// Shared implementation of the host and pre-trust bootstrap loads.
///
/// `bootstrap` selects the disposable pre-trust runtime: it dispatches
/// `project_trust` after loading and suppresses the mailbox-close
/// `session_shutdown`; a host load does neither.
async fn load_extension_runtime_impl(
    paths: &[PathBuf],
    config: ExtensionConfig,
    catalog: Option<&ModelCatalog>,
    bootstrap: bool,
) -> Option<ExtensionHandle> {
    if paths.is_empty() {
        return None;
    }
    // A CLI strict-validation handoff replaces the second full load: the
    // parked runtime already ran discovery, transpile, top-level evaluation
    // and flag application for this exact path set. Flag application is
    // repeated defensively (idempotent) so a future stash site that skipped
    // it can never silently drop the CLI's `--flag=value` values.
    //
    // A bootstrap load must never adopt the parked host runtime: it needs its
    // own disposable actor (so its drop cannot shut the host runtime down) and
    // it is the pass that owns the single `project_trust` dispatch.
    if !bootstrap {
        if let Some(runtime) = take_validated_extension_runtime(paths, &config) {
            let _ = apply_cli_extension_flags(Some(&runtime));
            // The validation load ran before the host's catalog existed; push
            // it now so handlers and later factory-free resolution see it. A
            // factory that resolved models during that first load cannot be
            // retroactively fixed (see `load_extension_runtime_strict`), but
            // the consuming host's own loads do not have that limitation.
            if let Some(catalog) = catalog {
                catalog.apply(&runtime);
            }
            return Some(runtime);
        }
    }
    let cwd_display = config.cwd.display().to_string();
    // The negative-cache gate reads the host mode after `config` has moved
    // into the runtime handle below.
    let config_mode = config.mode.clone();
    // Partition builtin pseudo-paths from on-disk file paths BEFORE starting
    // anything (upstream `loadExtensionPaths`): a plan with only
    // `builtin:<name>` entries needs the shared state/path plan, not the
    // QuickJS engine. `ExtensionHandle::start` constructs the actor (and with
    // it the QuickJS runtime + prelude, the ~10.6 MB builtin-only startup
    // cost), so it is reserved for plans that actually load JS.
    let (builtin_names, file_paths) = crate::plugins::partition_paths(paths);
    // Persistent negative cache (print/json hosts): paths whose fingerprint
    // still matches a recorded load failure never reach the runtime, so a
    // deterministically-broken extension (e.g. a native addon boundary) costs
    // one load per content change instead of one per process start. This runs
    // before the start decision too: a load list emptied by cached failures
    // needs no engine either.
    let (file_paths, cached_failures) =
        partition_cached_extension_failures(file_paths, &config_mode);
    let runtime = if file_paths.is_empty() {
        // Builtin-only plan: no actor thread, no QuickJS runtime. The handle
        // keeps the shared `RuntimeState` (builtin registrations, dispatch
        // state) and the recorded path plan the unified plugin host consumes.
        ExtensionHandle::start_plan_only(config)
    } else {
        let started = if bootstrap {
            ExtensionHandle::start_disposable(config)
        } else {
            ExtensionHandle::start(config)
        };
        match started {
            Ok(runtime) => runtime,
            Err(error) => {
                crate::ext_diag!("[ext] failed to start the extension runtime: {error}");
                return None;
            }
        }
    };
    runtime.set_path_plan(paths.to_vec());
    // BEFORE `load`: extension factories evaluate synchronously inside the
    // actor's load command and may resolve models there (`getModelsJson`).
    if let Some(catalog) = catalog {
        catalog.apply(&runtime);
    }
    {
        // `builtin:<name>` entries that name no registered builtin fail the
        // load with the upstream error text.
        let mut diagnostics = Vec::new();
        crate::plugins::unknown_builtin_errors(&builtin_names, &mut diagnostics);
        for diagnostic in &diagnostics {
            crate::ext_diag!("[ext] {}: {}", diagnostic.path, diagnostic.message);
        }
    }
    for (path, reason) in &cached_failures {
        crate::ext_diag!(
            "[ext] skipped (cached failure): {} ({})",
            path.display(),
            reason
        );
    }
    match runtime.load(&file_paths).await {
        Ok(result) => {
            for error in &result.errors {
                crate::ext_diag!(
                    "[ext] failed to load {}: {}",
                    error.path.display(),
                    error.error
                );
            }
            update_extension_failure_cache(&result);
        }
        Err(error) => crate::ext_diag!("[ext] extension load failed: {error}"),
    }
    // `project_trust`: the JS face is consulted ONLY by the pre-trust
    // bootstrap pass — exactly once per face, before the trust store /
    // `defaultProjectTrust` policy is read. The final host loads deliberately
    // stay silent: the bootstrap decision is recorded process-wide and every
    // trust consumer ([`self::load_settings_for_in`], the session snapshot,
    // the SDK builder) reads it through
    // [`crate::extensions::project_trust::resolve_project_trust`]. Native
    // plugins are fanned out once by their owning host (see
    // [`dispatch_project_trust`]).
    if bootstrap {
        // P2-EVENT3-02: this pass owns the only `project_trust` dispatch and
        // has no presentation sink (interactive/RPC configs carry
        // `has_ui: true`). Fail `ctx.ui.*` dialogs fast for the dispatch, then
        // clear the flag: the same handle may be adopted as the live host.
        runtime.set_ui_fail_fast(true);
        dispatch_project_trust(Some(&runtime), None, &cwd_display).await;
        runtime.set_ui_fail_fast(false);
    }
    // Extension-registered CLI flags: apply the unknown `--flag[=value]`
    // arguments the CLI collected (registered type decides the parse). The
    // CLI entry point validates the returned diagnostics separately.
    let _ = apply_cli_extension_flags(Some(&runtime));
    Some(runtime)
}

/// One extension load failure collected by [`load_extension_runtime_strict`]:
/// the failing path (`None` for runtime-level failures) plus the upstream
/// error line. The path lets the CLI classify failures — the user's own
/// paths are fatal, package-contributed paths are warn-only.
#[derive(Debug, Clone)]
pub struct ExtensionLoadFailure {
    pub path: Option<PathBuf>,
    pub message: String,
}

/// Strict variant of [`load_extension_runtime`] for the CLI hosts: identical
/// loading, but every load failure is ALSO collected and returned so the host
/// can fail the run with upstream's messaging instead of logging and
/// continuing.
///
/// Expected main.rs wiring (upstream `loadExtensionPaths` failure semantics):
///
/// ```ignore
/// let (extensions, failures) =
///     match crate::agent_session::load_extension_runtime_strict(&paths, config).await {
///         Ok(result) => result,
///         Err(_) => std::process::exit(1), // runtime itself failed to start
///     };
/// if failures.iter().any(|failure| /* not a package path */ true) {
///     for failure in &failures {
///         eprintln!("{}", failure.message);
///     }
///     eprintln!("Hint: Start without extensions using \"pir -ne\"");
///     std::process::exit(1);
/// }
/// ```
///
/// Each collected error line reads `Failed to load extension "<path>": <error>`
/// (upstream text; runtime-start failures surface as the `Err` arm).
pub async fn load_extension_runtime_strict(
    paths: &[PathBuf],
    config: ExtensionConfig,
) -> crate::Result<(Option<ExtensionHandle>, Vec<ExtensionLoadFailure>)> {
    load_extension_runtime_strict_with_catalog(paths, config, None).await
}

/// [`load_extension_runtime_strict`] with a model catalog pushed onto the
/// runtime BEFORE `load`. The CLI's early validation host runs before the
/// provider environment is loaded and therefore passes `None`; a host that
/// validates with a registry already in hand should use this variant so
/// factory-time model resolution sees the same catalog the session later
/// pushes.
pub async fn load_extension_runtime_strict_with_catalog(
    paths: &[PathBuf],
    config: ExtensionConfig,
    catalog: Option<&ModelCatalog>,
) -> crate::Result<(Option<ExtensionHandle>, Vec<ExtensionLoadFailure>)> {
    let mut failures: Vec<ExtensionLoadFailure> = Vec::new();
    if paths.is_empty() {
        return Ok((None, failures));
    }
    // The negative-cache gate reads the host mode after `config` has moved
    // into the runtime handle below.
    let config_mode = config.mode.clone();
    // Partition BEFORE starting the runtime: a builtin-only plan must not pay
    // for the QuickJS engine/actor (see `load_extension_runtime`).
    let (builtin_names, file_paths) = crate::plugins::partition_paths(paths);
    // Persistent negative cache (print/json hosts): a cached failure skips
    // the load but keeps its strict classification — the failure entry below
    // carries the recorded reason, so the CLI's fatal/warn gate sees the same
    // shape as a fresh failed load without paying for the probe again. The
    // start decision below uses the surviving load list.
    let (file_paths, cached_failures) =
        partition_cached_extension_failures(file_paths, &config_mode);
    let runtime = if file_paths.is_empty() {
        // Builtin-only plan: plan-only handle, no actor/QuickJS runtime.
        ExtensionHandle::start_plan_only(config)
    } else {
        match ExtensionHandle::start(config) {
            Ok(runtime) => runtime,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "failed to start the extension runtime: {error}"
                ));
            }
        }
    };
    runtime.set_path_plan(paths.to_vec());
    // BEFORE `load`: see `load_extension_runtime_with_catalog`.
    if let Some(catalog) = catalog {
        catalog.apply(&runtime);
    }
    {
        // `builtin:<name>` entries that name no registered builtin fail the
        // load with the upstream error text.
        let mut diagnostics = Vec::new();
        crate::plugins::unknown_builtin_errors(&builtin_names, &mut diagnostics);
        for diagnostic in &diagnostics {
            crate::ext_diag!("[ext] {}: {}", diagnostic.path, diagnostic.message);
            failures.push(ExtensionLoadFailure {
                path: Some(PathBuf::from(&diagnostic.path)),
                message: format!(
                    "Failed to load extension \"{}\": {}",
                    diagnostic.path, diagnostic.message
                ),
            });
        }
    }
    for (path, reason) in &cached_failures {
        crate::ext_diag!(
            "[ext] skipped (cached failure): {} ({})",
            path.display(),
            reason
        );
        failures.push(ExtensionLoadFailure {
            path: Some(path.clone()),
            message: format!(
                "Failed to load extension \"{}\": {}",
                path.display(),
                reason
            ),
        });
    }
    match runtime.load(&file_paths).await {
        Ok(result) => {
            for error in &result.errors {
                crate::ext_diag!(
                    "[ext] failed to load {}: {}",
                    error.path.display(),
                    error.error
                );
                failures.push(ExtensionLoadFailure {
                    path: Some(error.path.clone()),
                    message: format!(
                        "Failed to load extension \"{}\": {}",
                        error.path.display(),
                        error.error
                    ),
                });
            }
            update_extension_failure_cache(&result);
        }
        Err(error) => {
            crate::ext_diag!("[ext] extension load failed: {error}");
            failures.push(ExtensionLoadFailure {
                path: None,
                message: format!("Failed to load extension: {error}"),
            });
        }
    }
    // `project_trust` is deliberately NOT dispatched here: this is a final
    // (host) load, and the event is owned by the CLI's pre-trust bootstrap
    // pass ([`load_extension_runtime_bootstrap`]) so each face observes it
    // exactly once, before trust is resolved.
    let _ = apply_cli_extension_flags(Some(&runtime));
    Ok((Some(runtime), failures))
}

/// One diagnostic produced while wiring the CLI's collected
/// `--flag[=value]` arguments into the loaded extension runtime (upstream
/// `AgentSessionRuntimeDiagnostic`). `error` marks the upstream
/// `type: "error"` entries: an unregistered flag or a string flag without a
/// value aborts startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliExtensionFlagDiagnostic {
    pub error: bool,
    pub message: String,
}

impl CliExtensionFlagDiagnostic {
    fn error(message: String) -> Self {
        Self {
            error: true,
            message,
        }
    }
}

/// Apply the CLI's collected unknown `--flag[=value]` arguments to the
/// extension-registered flags (upstream `applyExtensionFlagValues`,
/// agent-session-services.ts:82-128): values are applied after load so factory
/// registrations exist, and every unregistered name / string flag without a
/// value becomes an error diagnostic.
///
/// `runtime` is `None` when the run has no extension plan at all: every
/// collected flag is then unregistered. The CLI entry point calls this once
/// after the strict validation load and exits 1 on any `error` diagnostic —
/// before, an unregistered `--x` was silently swallowed by the collector and
/// `continue`d over by the flag applier.
pub fn apply_cli_extension_flags(
    runtime: Option<&ExtensionHandle>,
) -> Vec<CliExtensionFlagDiagnostic> {
    let flags = crate::coding_agent::cli::collected_unknown_flags();
    if flags.is_empty() {
        return Vec::new();
    }
    let without_value = crate::coding_agent::cli::collected_unknown_flags_without_value();
    let registered = runtime.map(ExtensionHandle::flags).unwrap_or_default();
    let mut diagnostics: Vec<CliExtensionFlagDiagnostic> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for (name, value) in &flags {
        let flag_name = name.trim_start_matches('-');
        let Some(def) = registered.iter().find(|flag| flag.name == flag_name) else {
            unknown.push(format!("--{flag_name}"));
            continue;
        };
        let parsed = match def.flag_type.as_str() {
            "boolean" => match value.as_str() {
                "" | "true" => Some(crate::extensions::FlagValue::Bool(true)),
                "false" => Some(crate::extensions::FlagValue::Bool(false)),
                other => {
                    crate::ext_diag!(
                        "[ext] flag --{flag_name}: expected true/false, got {other:?}"
                    );
                    None
                }
            },
            _ => {
                if without_value.contains(name) {
                    // Upstream `Extension flag "--name" requires a value`: the
                    // bare `--name` arrives as boolean `true`, not a string.
                    diagnostics.push(CliExtensionFlagDiagnostic::error(format!(
                        "Extension flag \"--{flag_name}\" requires a value"
                    )));
                    None
                } else {
                    Some(crate::extensions::FlagValue::Str(value.clone()))
                }
            }
        };
        if let Some(parsed) = parsed {
            if let Some(runtime) = runtime {
                if let Err(error) = runtime.set_flag_value(flag_name, parsed) {
                    crate::ext_diag!("[ext] flag --{flag_name}: {error}");
                }
            }
        }
    }
    if !unknown.is_empty() {
        let label = if unknown.len() == 1 {
            "option"
        } else {
            "options"
        };
        diagnostics.push(CliExtensionFlagDiagnostic::error(format!(
            "Unknown {label}: {}",
            unknown.join(", ")
        )));
    }
    diagnostics
}

/// Dispatch the `project_trust` event to both plugin faces and record the
/// decision.
///
/// `runtime` is the JS face (only the disposable pre-trust bootstrap load —
/// [`load_extension_runtime_bootstrap`] — passes it, before any plugin host
/// exists; final host loads never dispatch this event). `host` is the unified
/// plugin host built from the final plan: its native plugins observe it through
/// [`crate::plugins::PluginHost::emit_native`] — the host's JS adapter is
/// skipped, so a host that also carries the same runtime handle never
/// dispatches the JS face twice.
///
/// Payload: `{type: "project_trust", cwd}`. The merged result is the upstream
/// [`ProjectTrustEventResult`]: `{trusted: "yes" | "no" | "undecided",
/// remember?: boolean}`. The first decided handler wins in the JS face (the
/// prelude stops the dispatch); a native plugin contributes when the JS face
/// is undecided and it writes `trusted` into the payload.
///
/// The decision now ACTUALLY applies (upstream `project-trust.ts:55-70`): it
/// is recorded process-wide for `cwd` and consulted by
/// [`load_settings_for_in`](../runtime_loader/fn.load_settings_for_in.html)
/// and the session snapshot through
/// [`crate::extensions::project_trust::resolve_project_trust`], ahead of the
/// trust store / `defaultProjectTrust` policy. `remember: true` persists the
/// decision to the ambient `<agent_dir>/trust.json` (`PIR_TRUST_PROJECT=1`
/// still overrides everything, and a `no` decision cannot override it).
///
/// Returns the decision so a caller can surface it; `Undecided` means every
/// handler fell through.
pub async fn dispatch_project_trust(
    runtime: Option<&ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    cwd_display: &str,
) -> crate::extensions::project_trust::ProjectTrustDecision {
    use crate::extensions::project_trust::ProjectTrustDecision;

    let mut payload = serde_json::json!({ "type": "project_trust", "cwd": cwd_display });
    let mut decision = ProjectTrustDecision::Undecided;
    let mut remember = false;
    if let Some(runtime) = runtime {
        match runtime.emit("project_trust", &mut payload).await {
            Ok(outcome) => {
                decision = ProjectTrustDecision::from_wire(outcome.result.get("trusted"));
                remember = outcome
                    .result
                    .get("remember")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                if decision != ProjectTrustDecision::Undecided {
                    crate::ext_diag_info!(
                        "[ext] project_trust: {} (remember: {remember})",
                        decision.wire_key()
                    );
                }
            }
            Err(error) => crate::ext_diag!("[ext] project_trust dispatch failed: {error}"),
        }
    }
    if let Some(host) = host {
        if host.has_native_handlers("project_trust") {
            host.emit_native("project_trust", &mut payload).await;
            // Native plugins cannot return a typed outcome through
            // `emit_native`; they contribute by writing the upstream fields
            // into the shared payload. A JS decision already made wins.
            if decision == ProjectTrustDecision::Undecided {
                decision = ProjectTrustDecision::from_wire(payload.get("trusted"));
                remember = remember
                    || payload
                        .get("remember")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
            }
        }
    }
    if let Some(trusted) = decision.trusted() {
        let pinned = crate::extensions::project_trust::is_decision_pinned(Path::new(cwd_display));
        crate::extensions::project_trust::record_decision(Path::new(cwd_display), decision);
        // The startup gate already pinned this run's boolean: a later load
        // (the final plan after the trust resolution, or a session reload)
        // must not persist a different decision (`--no-approve` with a
        // `remember: true` grant stays "untrusted" for this run). Only the
        // pre-trust bootstrap pass is allowed to write the store.
        if remember && !pinned {
            let agent_dir = crate::coding_agent::context::agent_dir();
            let store = crate::config::trust::ProjectTrustStore::new(&agent_dir);
            if let Err(error) = store.set(Path::new(cwd_display), Some(trusted)) {
                crate::ext_diag!("[ext] project_trust: cannot persist the decision: {error}");
            }
        }
    }
    decision
}

/// Outcome of the `user_bash` extension event for a `!`-prefixed command
/// (upstream `UserBashEventResult`); re-exported from
/// [`crate::extensions::UserBashOutcome`].
pub use crate::extensions::UserBashOutcome;

/// Dispatch the `user_bash` event for a `!`-prefixed TUI command (upstream
/// `emitUserBash`): `undefined`/no handlers means local execution; a valid
/// `{result}` object replaces the execution; `{operations}` carries custom
/// operations. A handler that throws or returns an invalid shape is reported
/// and the command is NOT run locally (upstream rethrows and the interactive
/// mode returns without executing, P1-1).
pub async fn dispatch_user_bash(
    runtime: &ExtensionHandle,
    command: &str,
    exclude_from_context: bool,
) -> UserBashOutcome {
    dispatch_user_bash_with_host(Some(runtime), None, command, exclude_from_context).await
}

/// [`dispatch_user_bash`] with the unified plugin host's native fan-out.
///
/// The event rides the host's ordered dispatch ([`dispatch_hosted_event`]):
/// the JS adapter is consulted once at its list position and every Rust
/// plugin observes the same `{type, command, excludeFromContext, cwd}` wire
/// payload, so a native handler can supply `{result}`/`{operations}` (or
/// `{aborted: true}`) exactly like a JS one. `host == None` is the historical
/// JS-only route.
pub async fn dispatch_user_bash_with_host(
    runtime: Option<&ExtensionHandle>,
    host: Option<&crate::plugins::PluginHost>,
    command: &str,
    exclude_from_context: bool,
) -> UserBashOutcome {
    let wants = runtime.is_some_and(|runtime| runtime.has_handlers("user_bash"))
        || host.is_some_and(|host| host.has_handlers("user_bash"));
    if !wants {
        return UserBashOutcome::Local;
    }
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string();
    let mut payload = serde_json::json!({
        "type": "user_bash",
        "command": command,
        "excludeFromContext": exclude_from_context,
        "cwd": cwd,
    });
    match crate::agent::tool_execution::dispatch_hosted_event(
        runtime,
        host,
        "user_bash",
        &mut payload,
    )
    .await
    {
        Ok(outcome) => {
            // A handler threw or returned an invalid shape (reported by the
            // dispatch layer): abort instead of running locally (P1-1).
            if !outcome.errors.is_empty()
                || outcome.result.get("aborted").and_then(JsonValue::as_bool) == Some(true)
            {
                return UserBashOutcome::Aborted;
            }
            let result = &outcome.result;
            if let Some(bash_result) = result.get("result").filter(|value| value.is_object()) {
                let output = bash_result
                    .get("output")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let is_error = bash_result
                    .get("isError")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or_else(|| bash_result.get("error").is_some_and(|e| !e.is_null()));
                return UserBashOutcome::Handled { output, is_error };
            }
            if let Some(operations) = result.get("operations").filter(|value| value.is_object()) {
                return UserBashOutcome::Operations {
                    operations: operations.clone(),
                };
            }
            // No handler produced a decision.
            UserBashOutcome::Local
        }
        Err(error) => {
            crate::ext_diag!("[ext] user_bash dispatch failed: {error}");
            UserBashOutcome::Aborted
        }
    }
}
