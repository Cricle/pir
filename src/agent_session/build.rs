use super::*;

// Live host-service binding for native plugins (`PluginContext` built from
// this session's typed cells). `build.rs` is not a `mod.rs`, so a nested
// module lives under `build/` by default; the explicit path keeps the sibling
// file at `src/agent_session/plugin_services.rs`.
#[path = "plugin_services.rs"]
pub(super) mod plugin_services;

/// Session teardown aborts every in-flight background tool call
/// (`TOOL-BACKGROUND-SCOPE.md` §2.5): a detached call outlives its turn,
/// never its session. The registry holds each call's OWN abort signal (never
/// the run signal), and the completion task deregisters before delivering, so
/// a completion racing the teardown writes nothing.
impl Drop for AgentSession {
    fn drop(&mut self) {
        let cancelled = self.background.abort_all();
        if cancelled > 0 {
            tracing::debug!(
                cancelled,
                "aborted in-flight background tool calls on session drop"
            );
        }
    }
}

/// Dispatch `project_trust` to the NATIVE plugins of a freshly built host.
///
/// The runtime loader already consulted the JS face at load time — before the
/// unified host existed — so this is the missing native half. [`PluginHost::emit_native`]
/// skips the host's JS adapter, so a host that also carries the same runtime
/// handle never dispatches the JS face twice.
///
/// Session construction is synchronous, so the event is spawned on the current
/// runtime instead of awaited: construction never blocks on a plugin handler,
/// and a host built with no runtime available (a plain sync embedder) skips the
/// fan-out with a debug log rather than panicking. Ordering caveat: the handler
/// therefore runs after `new_with_plugins` returns and may race a very fast
/// first `session_start`/prompt, or (on a multi-thread runtime) the
/// `bind_context` pass that follows host construction; `project_trust` is
/// informational (pir's trust decision stays host-owned, see
/// [`dispatch_project_trust`]), so the later delivery is harmless.
fn spawn_native_project_trust(host: &Arc<PluginHost>, cwd: Option<&Path>) {
    if !host.has_native_handlers("project_trust") {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::debug!(
            "no tokio runtime at session construction; skipping the native project_trust fan-out"
        );
        return;
    };
    let host = Arc::clone(host);
    let cwd = cwd
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .display()
        .to_string();
    runtime.spawn(async move {
        let mut payload = serde_json::json!({ "type": "project_trust", "cwd": cwd });
        host.emit_native("project_trust", &mut payload).await;
    });
}

impl AgentSession {
    /// Build a session facade from configuration.
    pub fn new(config: AgentSessionConfig) -> Self {
        Self::new_with_plugins(config, Vec::new())
    }

    /// Build a session facade bound to an EXPLICIT agent directory (D1).
    ///
    /// The directory becomes the session's identity for every session-scoped
    /// agent-dir read: credentials ([`AgentSession::auth_storage`]), the merged
    /// settings ([`AgentSession::session_settings`]), the sessions root
    /// ([`AgentSession::sessions_dir`]), and the builtin/settings reads the
    /// build itself performs. It is resolved ONCE here, so a later
    /// `PIR_AGENT_DIR` change in the process environment never re-points a
    /// live session.
    ///
    /// This is an OVERRIDE, not a replacement for environment resolution:
    /// [`AgentSession::new`] keeps resolving `PIR_AGENT_DIR` /
    /// `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR` (else `~/.pi/agent`)
    /// once at build, so a fresh process and a fresh session behave exactly as
    /// before.
    pub fn new_with_agent_dir(config: AgentSessionConfig, agent_dir: impl Into<PathBuf>) -> Self {
        Self::new_with_plugins_and_agent_dir(config, Vec::new(), Some(agent_dir.into()))
    }

    /// Tear the session's plugin surface down: run every plugin's
    /// `on_shutdown` hook exactly once (idempotent — later calls are no-ops).
    /// Hosts call this when the session ends (TUI worker exit, RPC server
    /// shutdown, child run end); it is the path that makes the `mcp`
    /// builtin's graceful stdio close and process-tree sweep reachable
    /// outside the SDK's explicit `shutdown()` (sdk/agent.rs).
    pub async fn shutdown(&self) {
        if let Some(plugins) = &self.plugins {
            plugins.shutdown().await;
        }
    }

    /// Build a session facade from configuration plus a list of inline Rust
    /// plugins ([`crate::plugins::AgentPlugin`] implementations, appended last
    /// on the unified dispatch — upstream `<inline:...>` factories).
    ///
    /// This is the seam embedders (the SDK facade) use to attach Rust plugins
    /// without a JS extension runtime: the plugin host is built whenever
    /// inline plugins are present, even when `config.extensions` is `None`.
    /// With inline plugins attached, live tool partial updates
    /// (`tool_execution_update`) dispatch through the unified plugin host so
    /// Rust plugins observe them; JS-only sessions keep the direct runtime
    /// path.
    ///
    /// Construction ends with the live [`PluginContext`](crate::plugins::plugin::PluginContext)
    /// bound to the host (status/events/flags/provider services plus every
    /// session read the by-value form can back). Use
    /// [`AgentSession::new_arc_with_plugins`] when plugins need the
    /// session-identity reads (`session_id`, `model`, ...): only an `Arc`
    /// session can hand the adapters a `Weak`.
    pub fn new_with_plugins(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
    ) -> Self {
        Self::new_with_plugins_and_agent_dir(config, inline_plugins, None)
    }

    /// [`Self::new_with_plugins`] with an explicit agent-directory override
    /// (see [`Self::new_with_agent_dir`]).
    fn new_with_plugins_and_agent_dir(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        agent_dir: Option<PathBuf>,
    ) -> Self {
        let (session, services) = Self::build_with_live_services(config, inline_plugins, agent_dir);
        if let (Some(plugins), Some(services)) = (session.plugins.as_ref(), services) {
            plugins.bind_context(services.into_context());
        }
        session
    }

    /// Build the session and its live service set WITHOUT binding. Binding
    /// happens exactly once in the public constructor that owns the final
    /// session form: [`Self::new_with_plugins`] (by value) or
    /// [`Self::new_arc_with_plugins`] (after the `Arc` exists, so the weak
    /// session slot is live before `on_context` fires).
    fn build_with_live_services(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        agent_dir: Option<PathBuf>,
    ) -> (Self, Option<plugin_services::LiveSessionServices>) {
        Self::build_with_live_services_and_host(config, inline_plugins, None, agent_dir)
    }

    /// Build the unified plugin host for a session configuration: enabled Rust
    /// builtins (from the `builtin:<name>` entries in the runtime's recorded
    /// path plan) plus the JS adapter, sharing ONE registry and ONE event
    /// dispatch, plus any inline Rust plugins appended last.
    ///
    /// Exposed to hosts that must inspect the merged tool registry BEFORE the
    /// session exists (the SDK builder resolves its tool selection against it)
    /// and then hand the exact host to [`Self::new_arc_with_plugins_and_host`],
    /// so plugin factories and their registration pass run once.
    #[cfg(feature = "sdk")]
    pub(crate) fn build_plugin_host(
        extensions: Option<&crate::extensions::ExtensionHandle>,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        cwd: Option<&Path>,
    ) -> Arc<PluginHost> {
        Self::build_plugin_host_in(extensions, inline_plugins, cwd, None)
    }

    /// [`Self::build_plugin_host`] with an explicit agent directory for the
    /// settings-driven builtin gates (`None` resolves the process environment
    /// once, the pre-D1 behavior).
    pub(crate) fn build_plugin_host_in(
        extensions: Option<&crate::extensions::ExtensionHandle>,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        cwd: Option<&Path>,
        agent_dir: Option<&Path>,
    ) -> Arc<PluginHost> {
        // The settings-driven builtin gates (`pythonTool: false` and
        // `disabledBuiltinExtensions`) drop their paths from the recorded
        // plan BEFORE `PluginHost::build` below can instantiate them — through
        // the ONE helper the print host calls too, so the two host paths
        // cannot drift again (print used to build its host directly and
        // silently ignore `pythonTool: false`).
        if let Some(extensions) = extensions {
            let settings = Self::host_settings_in(cwd, agent_dir);
            Self::restrict_plan_for_builtins(extensions, &settings);
        }
        let host = Arc::new(PluginHost::build(extensions.cloned(), inline_plugins));
        for diagnostic in &host.registry().diagnostics().to_vec() {
            report_plugin_diagnostic(diagnostic);
        }
        // The phone builtin (when the plan contains it) gets the live
        // session adapter installed here, at construction. The adapter is
        // bound to the session by `install_phone_host`, which the host
        // calls once the session is reachable as an `Arc`
        // (`new_arc_with_plugins` does it automatically).
        #[cfg(feature = "phone")]
        install_phone_plugin_adapter(&host);
        // Native half of the startup `project_trust` fan-out: the runtime
        // loader already dispatched the event to the JS face while loading,
        // BEFORE this host existed. `emit_native` skips the host's JS
        // adapter, so the JS face is never consulted twice.
        spawn_native_project_trust(&host, cwd);
        host
    }

    /// The settings snapshot a host build resolves its builtin gates from.
    ///
    /// ONE resolution for every host: the global agent directory plus
    /// `<cwd>/.pi`, project scope only when trusted (the same
    /// `runtime_loader::load_settings_for` the session host has always used).
    /// The print host calls this too, instead of re-deriving the scopes, so
    /// `pythonTool`/`disabledBuiltinExtensions` cannot be read differently in
    /// interactive, RPC, SDK and print.
    #[cfg(feature = "print")]
    pub(crate) fn host_settings(cwd: Option<&Path>) -> SettingsManager {
        Self::host_settings_in(cwd, None)
    }

    /// [`Self::host_settings`] against an explicit agent directory (`None`
    /// resolves the process environment once).
    pub(crate) fn host_settings_in(
        cwd: Option<&Path>,
        agent_dir: Option<&Path>,
    ) -> SettingsManager {
        let assembly_cwd = cwd
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        match agent_dir {
            Some(agent_dir) => runtime_loader::load_settings_for_in(agent_dir, &assembly_cwd),
            None => load_settings_for(&assembly_cwd),
        }
    }

    /// Drop the builtins a settings snapshot disables from a runtime's
    /// recorded plan, BEFORE a host instantiates them: the
    /// `disabledBuiltinExtensions` names plus `python` when
    /// `pythonTool: false` (folded into one set by
    /// [`SettingsManager::disabled_builtins`]).
    ///
    /// This is the ONE implementation both host builders call —
    /// [`Self::build_plugin_host`] (interactive/RPC/SDK) and
    /// [`crate::coding_agent::print::print_plugin_host`] — so a settings key
    /// that works in one host cannot silently miss another (the
    /// `pythonTool: false` defect: print built its host directly). It reuses
    /// the loader's [`crate::plugins::filter_disabled_builtins`], so the same
    /// `tool-repair`/phone precedence rules apply as at plan-build time.
    /// Idempotent, and it never re-enables anything: settings only ADD names.
    pub(crate) fn restrict_plan_for_builtins(
        extensions: &crate::extensions::ExtensionHandle,
        settings: &SettingsManager,
    ) {
        let disabled = settings.disabled_builtins();
        if disabled.is_empty() {
            return;
        }
        let plan = extensions.path_plan();
        let filtered = crate::plugins::filter_disabled_builtins(&plan, &disabled);
        if filtered != plan {
            extensions.set_path_plan(filtered);
        }
    }

    fn build_with_live_services_and_host(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        prepared_host: Option<Arc<PluginHost>>,
        agent_dir: Option<PathBuf>,
    ) -> (Self, Option<plugin_services::LiveSessionServices>) {
        let has_inline_plugins = !inline_plugins.is_empty();
        // D1: the session's agent-directory identity is resolved EXACTLY ONCE,
        // here — the caller's explicit override, else the process environment
        // at build time. Everything built below (the plugin host's builtin
        // gates, the settings snapshot, and the session's own reads) uses this
        // value, so a mid-process `PIR_AGENT_DIR` change cannot split the live
        // session across two directories.
        let agent_dir = agent_dir.unwrap_or_else(crate::ai::providers::agent_dir);
        // A caller-supplied host already owns the inline plugin list (the SDK
        // builder built it to resolve tool selection against the merged
        // registry); never construct a second one.
        let wants_host =
            prepared_host.is_some() || config.extensions.is_some() || has_inline_plugins;
        let plugins = if wants_host {
            Some(match prepared_host {
                Some(host) => host,
                None => Self::build_plugin_host_in(
                    config.extensions.as_ref(),
                    inline_plugins,
                    config.cwd.as_deref(),
                    Some(&agent_dir),
                ),
            })
        } else {
            None
        };
        let session = Self::build(config, plugins, has_inline_plugins, agent_dir);
        let services = session
            .plugins
            .as_ref()
            .map(|plugins| plugin_services::live_services(&session, plugins));
        (session, services)
    }

    fn build(
        config: AgentSessionConfig,
        plugins: Option<Arc<PluginHost>>,
        has_inline_plugins: bool,
        agent_dir: PathBuf,
    ) -> Self {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let (session_id, session_path) = match &config.session {
            Some(session) => (
                Some(session.id().to_owned()),
                Some(session.path().to_path_buf()),
            ),
            None => (None, None),
        };
        // Base (core) tools; plugin tools (Rust builtins + JS extensions)
        // merge lazily per prompt run through the unified registry (see
        // [`AgentSession::merged_tool_registry`]). The host-supplied selection
        // is ONLY the active override: the registry stays the full registered
        // set (the same F12 rule the merged plugin registry follows), so an
        // allowlisted `codemode`/`deferred` tool and any tool restored from a
        // transcript remain reachable. Hidden tools are still dropped.
        let tool_selection = config.tool_selection;
        let mut tools = config.tools;
        tools.remove_exposure(crate::agent::tool_execution::ToolExposure::Hidden);
        // Session resume (upstream `_restoreToolsFromTranscript`, gated on
        // `usesDefaultTools`; 6f2f164ab → agent-session.ts:523-527): a session
        // that uses the default tools replaces them with the loadout its
        // transcript records, with the selection's `+name`/`-name` entries
        // applied; an explicit selection keeps the base. The registry above is
        // the full base set, so every recorded name that is still registered
        // can be restored.
        // Paged resume: the tool-loadout record may start in the SKIPPED
        // PREFIX of a tail-first session. `recorded_tool_loadout` reads only
        // SYSTEM messages, so only those are materialized — the prefix's
        // projection system messages (the compaction replays and declarations)
        // are STREAMED in file order and the resident window contributes its
        // system declarations from projection METADATA. The full transcript is
        // never materialized here: the first prompt builds the one projection
        // the run needs, and a construction-time copy only doubled it (then
        // kept it resident in `self.history`, which a persisted session never
        // reads). An explicit tool selection skips the restore entirely
        // (`restore_recorded_active_tools` returns it unchanged), so no scan
        // runs either.
        let restore_recorded_loadout = tool_selection
            .as_ref()
            .is_none_or(|selection| selection.uses_default_tools);
        let tool_history: Vec<crate::ai::types::Message> =
            match (&config.session, restore_recorded_loadout) {
                (Some(session), true) => session_recorded_system_messages(session),
                _ => Vec::new(),
            };
        let tool_selection = tools::restore_recorded_active_tools(tool_selection, &tool_history);
        // Context-file injection for hosts that pass no explicit system
        // prompt (RPC mode; the SDK's append-only form, where the addendum is
        // applied to this prompt afterwards): the full structured system
        // prompt is ALWAYS built via the ordered section builder, with
        // AGENTS.md/CLAUDE.md files (unless disabled via --no-context-files /
        // PIR_NO_CONTEXT_FILES) as the `project_context` section. Only an
        // explicit `system_prompt: Some(..)` replaces that derived prompt;
        // an empty/absent file set no longer produces "no prompt".
        //
        // D6: the prompt's tool list is the DECLARED (active) set — upstream
        // `_rebuildSystemPrompt` passes `selectedTools: validToolNames`
        // (agent-session.ts:1705-1731) — not the base registry. The merged
        // registry below is the composition
        // [`AgentSession::merged_tool_registry`] runs per prompt (base +
        // plugin tools, the JS `setActiveTools` store, `hidden` dropped), so
        // `--tools ctx_execute` lists exactly the tools the request declares
        // and never advertises an un-declared core tool. No selection still
        // starts from the same base registry and lists every
        // registration-active default (including the default-active plugin
        // tools the request declares); a default-inactive core tool
        // (`powershell`) is no longer advertised because it is never declared.
        //
        // The prompt also applies each declared tool's Rust `prepareLoadout`
        // hidden declarations (upstream `hiddenTools: [...this._hiddenDeclarations]`,
        // agent-session.ts:1727): with `codemode.mode: "only"` the direct
        // tools stay active and callable (scripts reach them) but leave the
        // `<tools>`/`<rules>` sections and the skills reader hint, exactly as
        // they leave the request declarations through
        // [`AgentSession::build_tool_loadout`] — print's prompt does the same
        // (`print.rs`). JS `prepareLoadout` hooks are async and this build is
        // synchronous: their hidden declarations reach the per-request
        // declarations but not this prompt (documented limitation; upstream's
        // hooks are synchronous).
        //
        // Ambient boundary: the session build is where this process's
        // environment is legitimately read (the cwd fallback above does the
        // same), so the `context_system_prompt` boundary wrapper owns the
        // PIR_NO_CONTEXT_FILES / agent-dir / package-dir reads. The agent
        // directory is the exception: it was already resolved once above (D1),
        // so the explicit [`ContextOptions`] core is called with THAT value —
        // global context files and `<agentDir>/skills` stay tied to the
        // session's identity even if `PIR_AGENT_DIR` changes mid-process.
        // The package asset dir is resolved ONCE here, like the agent dir: the
        // build, the `/reload` rebuild and the runtime active-set rebuild all
        // use this value, so a mid-process `PI_PACKAGE_DIR` change cannot split
        // the session's prompt across two installations (D1's rule applied to
        // the prompt's `docs` section).
        let package_dir = crate::config::env::package_dir();
        // The discovery inputs the prompt is built from: cached so a later
        // active-set change (tool_search / MCP discovery / `setActiveTools`)
        // can rebuild the `<tools>`/`<rules>` sections without re-reading
        // context files or skills (D6; upstream re-runs `_rebuildSystemPrompt`
        // from the resource loader's loaded inputs, agent-session.ts:1705-1731).
        let mut prompt_context = None;
        let preamble_override: Option<String> = None;
        let append_sections: Vec<String> = Vec::new();
        let system_prompt = config.system_prompt.clone().or_else(|| {
            let host_override = plugins
                .as_deref()
                .and_then(PluginHost::active_tools_override);
            let override_names = host_override.clone().or_else(|| {
                tool_selection
                    .as_ref()
                    .map(|selection| selection.enabled.clone())
            });
            let (prompt_tools, hidden_tools, active) = match plugins.as_deref() {
                Some(plugins) => {
                    let mut registry = tools.clone();
                    crate::agent::tool_execution::merge_plugin_tools(
                        &mut registry,
                        plugins,
                        host_override.as_deref(),
                    );
                    registry.remove_exposure(crate::agent::tool_execution::ToolExposure::Hidden);
                    let active = registry.active_names(override_names.as_deref());
                    (
                        declared_tool_infos(&registry, &active),
                        rust_hidden_declarations(&registry, &active),
                        active,
                    )
                }
                None => {
                    let active = tools.active_names(override_names.as_deref());
                    (
                        declared_tool_infos(&tools, &active),
                        rust_hidden_declarations(&tools, &active),
                        active,
                    )
                }
            };
            let (prompt, inputs) = crate::coding_agent::context::
                session_system_prompt_with_hidden_and_options_and_inputs_and_mcp(
                    &cwd,
                    &prompt_tools,
                    &hidden_tools,
                    &crate::coding_agent::context::ContextOptions {
                        disabled: !crate::coding_agent::context::context_files_enabled(),
                        agent_dir: &agent_dir,
                        package_dir: package_dir.as_deref(),
                    },
                    // Upstream renders the `mcp_servers` section on every
                    // `before_agent_start` (extensions/mcp/index.ts:1176-1184);
                    // pir caches it with the other discovered inputs so a
                    // steady turn does no MCP IO. A status transition lands on
                    // the next build/reload (`sync_context_prompt` /
                    // `/reload`), never mid-prompt.
                    crate::coding_agent::context::mcp_servers_prompt_section(
                        plugins.as_deref(),
                    ),
                );
            // P7/U4: the SESSION hosts always send the base prompt (preamble,
            // `<tools>`/`<rules>`, `<cwd>`) — upstream `buildSystemPrompt`
            // (system-prompt.ts:60-199) has no "nothing to inject" arm, but the
            // shared builder used to return `None` on a bare workspace, so the
            // model got no tool list or rules at all. The cache is installed
            // unconditionally so later active-set changes keep the prompt in
            // sync (`sync_context_prompt`).
            prompt_context = Some(PromptContextCache {
                inputs,
                active,
                hidden: hidden_tools.into_iter().collect(),
                append: None,
                preamble_override,
                append_sections,
            });
            Some(prompt)
        });
        // Virtual models persisted on the session branch
        // (`pi.virtual-model-state` custom entries) re-register into the
        // provider registry's overlay so resume keeps the selection routable.
        restore_session_virtual_models(&config.provider_registry, &config.session);
        // `pi.getSettings()`: snapshot the merged effective settings (global
        // scope + trusted project scope) into the runtime state. The facade
        // reads the same public SettingsManager the CLI uses; disk IO is two
        // bounded settings.json reads. The persisted queue modes apply to the
        // loop (upstream `syncQueueModesFromSettings`).
        let mut steering_mode = SteeringMode::OneAtATime;
        let mut follow_up_mode = FollowUpMode::OneAtATime;
        if let Some(extensions) = &config.extensions {
            let mut manager = runtime_loader::load_settings_for_in(&agent_dir, &cwd);
            for error in manager.drain_errors() {
                crate::ext_diag!("[ext] settings {}: {}", error.scope_name(), error.message);
            }
            steering_mode = manager.steering_mode();
            follow_up_mode = manager.follow_up_mode();
            extensions.update_settings(manager.raw);
        }
        // Background tool calls (§2.3): ONE registry per session plus a
        // delivery bundle of owned handles (no `Arc<Self>` self-reference),
        // installed as the scheduler's detach sink here — before the scheduler
        // is shared — so every execution path of the session (model batches
        // and nested calls) adopts `background: true` calls without a
        // signature change. Non-background calls never touch any of this.
        let session_handle = config
            .session
            .map(|session| Arc::new(tokio::sync::Mutex::new(session)));
        let session_events_handle = Arc::new(std::sync::RwLock::new(Vec::new()));
        let follow_up_inbox_handle = Inbox::new();
        let background_follow_up_inbox_handle = Inbox::new();
        let session_state_handle = Arc::new(AtomicU8::new(SESSION_IDLE));
        // The owning run's settling flag, shared with the background delivery
        // (a completion landing after the loop's terminal result is persisted
        // but NOT queued: no poll is left, and queueing would deliver the same
        // frame a second time on the next prompt).
        let run_settling_handle = Arc::new(AtomicBool::new(false));
        let background = Arc::new(background::BackgroundRegistry::new(
            background::DEFAULT_BACKGROUND_CALL_LIMIT,
        ));
        // The session host's persist sink: append the `backgroundToolResult`
        // custom entry to the session manager (the store-less session keeps
        // none — the event is its only surface). Same append semantics the
        // print host installs over its `SharedSession` (`run_print`).
        let background_persist: Option<background::BackgroundPersistFn> =
            session_handle.as_ref().map(|session| {
                let session = Arc::clone(session);
                let persist: background::BackgroundPersistFn = Arc::new(move |framed, details| {
                    let session = Arc::clone(&session);
                    Box::pin(async move {
                        let mut session = session.lock().await;
                        if let Err(error) = session.append_custom_message_entry(
                            background::BACKGROUND_TOOL_RESULT_TYPE,
                            JsonValue::String(framed),
                            false,
                            Some(details),
                        ) {
                            tracing::warn!(
                                error = %error,
                                "failed to persist a background tool result"
                            );
                        }
                    })
                });
                persist
            });
        let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(
            background::background_sink(
                Arc::clone(&background),
                background::BackgroundDelivery::new(
                    background_persist,
                    Arc::clone(&session_events_handle),
                    background_follow_up_inbox_handle.clone(),
                    Arc::clone(&session_state_handle),
                    config.extensions.clone(),
                )
                .with_plugins(plugins.clone())
                .with_run_settling(Arc::clone(&run_settling_handle)),
            ),
        );
        // The captured key belongs to the model the session is built with (a
        // models.json `apiKey` resolved for its provider, an SDK/CLI inline
        // key for the selected model): scoping it here is what keeps a later
        // `/model` switch from sending it to a DIFFERENT provider, which used
        // to come back as a 401 even though every provider's own key was
        // correct.
        let api_key = config
            .api_key
            .map(|key| stream::ProviderApiKey::new(config.model.provider.clone(), key));
        Self {
            registry: config.provider_registry,
            model: RwLock::new(config.model),
            extension_api_keys: Mutex::new(HashMap::new()),
            api_key,
            thinking: RwLock::new(config.thinking),
            tools: Arc::new(tools),
            tool_selection,
            active_tools_override: Arc::new(std::sync::RwLock::new(None)),
            host_tool_updates: Arc::new(Mutex::new(None)),
            scheduler,
            background,
            session: session_handle,
            session_id: RwLock::new(session_id),
            session_path: RwLock::new(session_path),
            session_storage_generation: AtomicU64::new(0),
            session_name: RwLock::new(None),
            // Ephemeral transcript only. A persisted session's history is the
            // session projection, built on demand by the run; holding a
            // construction-time copy here was a full transcript clone that no
            // reader of a store-backed session ever touched.
            history: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            compaction: Arc::new(RwLock::new(config.compaction)),
            max_turns: config.max_turns,
            cwd,
            system_prompt: std::sync::RwLock::new(system_prompt),
            prompt_context: std::sync::RwLock::new(prompt_context),
            package_dir,
            event_stream: config.event_stream,
            extensions: config.extensions,
            plugins,
            session_start_sent: AtomicBool::new(false),
            #[cfg(feature = "mcp")]
            mcp_unreachable_warned: AtomicBool::new(false),
            merged_extension_tools: Mutex::new(None),
            steering_inbox: Inbox::new(),
            follow_up_inbox: follow_up_inbox_handle,
            background_follow_up_inbox: background_follow_up_inbox_handle,
            steering_mode: Arc::new(RwLock::new(steering_mode)),
            follow_up_mode: Arc::new(RwLock::new(follow_up_mode)),
            bash_abort: Mutex::new(Vec::new()),
            bash_abort_seq: AtomicU64::new(0),
            pending_bash: Mutex::new(Vec::new()),
            auto_retry: AtomicBool::new(true),
            branch_summary_abort: BranchSummaryAbort::default(),
            retry_cancel: CancelLatch::default(),
            host_retry_notice: Mutex::new(None),
            signal: Mutex::new(AbortSignal::new()),
            turns: AtomicUsize::new(0),
            tokens: AtomicU64::new(0),
            session_state: session_state_handle,
            run_abort_requested: Arc::new(AtomicBool::new(false)),
            run_settling: run_settling_handle,
            idle_notify: Arc::new(Notify::new()),
            pending_custom_runs: Arc::new(Mutex::new(VecDeque::new())),
            pending_next_turn_messages: Arc::new(Mutex::new(Vec::new())),
            pending_setup_messages: Arc::new(Mutex::new(Vec::new())),
            pending_messages: Arc::new(AtomicUsize::new(0)),
            prepare_request: RwLock::new(None),
            finish_turn: RwLock::new(None),
            tool_hooks: RwLock::new(None),
            has_inline_plugins,
            stream_overrides: RwLock::new(StreamRetryOverride::default()),
            cache_warming: RwLock::new(None),
            warm_abort: Mutex::new(None),
            skills: std::sync::RwLock::new(Vec::new()),
            prompt_templates: std::sync::RwLock::new(Vec::new()),
            extension_resource_contribution: std::sync::RwLock::new(
                session_ctx::ExtensionResourceContribution::default(),
            ),
            context_derived_prompt: config.system_prompt.is_none(),
            scoped_models: RwLock::new(Vec::new()),
            session_events: session_events_handle,
            compacting: Arc::new(AtomicBool::new(false)),
            auto_compaction_cancel: Arc::new(Mutex::new(None)),
            self_weak: RwLock::new(None),
            queue_mirror: Arc::new(Mutex::new(QueueMirror::default())),
            last_run_diagnostics: std::sync::RwLock::new(None),
            session_context_cache: Mutex::new(None),
            // Read the ambient child flag once here (the process was spawned
            // by the phone pool with `PI_PHONE_CHILD=1`); every prompt then
            // checks a bool. Only the RPC host reads it.
            #[cfg(feature = "rpc")]
            phone_child: phone_child::child_mode_from_env(),
            // Native plugin actions (`MessageService`) queue the same JSON
            // payloads the JS host functions produce; the next
            // `apply_extension_notifications` drain applies them.
            native_actions: Arc::new(Mutex::new(VecDeque::new())),
            native_action_flush: tokio::sync::Mutex::new(()),
            agent_dir,
        }
    }

    /// Install the discovered skills and prompt templates backing the
    /// `/skill:<name>` and `/template` prompt-command expansion (upstream
    /// `session.resourceLoader` / `session.promptTemplates`). Additive seam:
    /// the facade itself never reads `~/.pi`; the host discovers the
    /// resources and installs them once at startup.
    pub fn set_prompt_resources(
        &mut self,
        skills: Vec<crate::skills::SkillInfo>,
        prompt_templates: Vec<crate::coding_agent::prompt_templates::PromptTemplate>,
    ) {
        // Startup install: the session build read the context files moments
        // ago, so the prompt re-render reuses those cached inputs instead of
        // walking the context files a second time. `/reload` keeps the disk
        // refresh (see `set_prompt_resources_shared`).
        Self::install_prompt_resources(self, skills, prompt_templates, false);
    }

    /// The discovered skills and prompt templates the session currently holds
    /// (`/skill:` and `/template` expansion sources). Read-only snapshot: the
    /// host installs them at startup ([`Self::set_prompt_resources`]) and a
    /// reload refreshes them in place.
    pub fn prompt_resources(
        &self,
    ) -> (
        Vec<crate::skills::SkillInfo>,
        Vec<crate::coding_agent::prompt_templates::PromptTemplate>,
    ) {
        (
            self.skills
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            self.prompt_templates
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

    /// Arc-safe twin of [`Self::set_prompt_resources`]: refreshes the
    /// discovered skills and prompt templates of the RUNNING session (upstream
    /// `session.reload` re-discovers resources; the TUI's `/reload` drives
    /// this through its worker command so `/skill:`/`/template` expansion and
    /// the system prompt's skills section update without a restart).
    pub fn set_prompt_resources_shared(
        &self,
        skills: Vec<crate::skills::SkillInfo>,
        prompt_templates: Vec<crate::coding_agent::prompt_templates::PromptTemplate>,
    ) {
        Self::install_prompt_resources(self, skills, prompt_templates, true);
    }

    /// Shared body of the two resource-install seams.
    ///
    /// `refresh_context_inputs` distinguishes the startup install (reuse the
    /// context files the build already read) from `/reload` (re-read them: a
    /// reload is a disk refresh). Whenever the installed skills and the MCP
    /// section are unchanged and no disk refresh was requested, the live
    /// prompt already renders exactly these inputs and nothing is rebuilt —
    /// prompt templates never enter the system prompt.
    fn install_prompt_resources(
        &self,
        skills: Vec<crate::skills::SkillInfo>,
        prompt_templates: Vec<crate::coding_agent::prompt_templates::PromptTemplate>,
        refresh_context_inputs: bool,
    ) {
        let skills_changed = {
            let current = self
                .skills
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current != skills
        };
        *self
            .skills
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = skills;
        *self
            .prompt_templates
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prompt_templates;
        // The `skills` section of the default structured prompt is built from
        // the same discovered list (upstream `formatSkillsForPrompt`), so a
        // host that installed `--skill` paths must see them there too.
        if !self.context_derived_prompt {
            return;
        }
        let skills = self
            .skills
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mcp_servers =
            crate::coding_agent::context::mcp_servers_prompt_section(self.plugins.as_deref());
        // A repeated install with identical resources has nothing to render.
        // (`/reload` keeps `refresh_context_inputs` true so an edited context
        // file still lands even when the discovered skill list is unchanged.)
        if !refresh_context_inputs {
            let cache = self
                .prompt_context
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(cache) = cache.as_ref() else {
                return;
            };
            if !skills_changed
                && cache.inputs.skills == skills
                && cache.inputs.mcp_servers == mcp_servers
            {
                return;
            }
        }
        // D6: the reload rebuilds the prompt from the same DECLARED set the
        // build used (upstream `_rebuildSystemPrompt` re-runs on every
        // active-set change), so a `/reload` cannot reintroduce the base
        // registry's tools or drop the selected plugin tools. The live
        // registry/active snapshot also carries a `setActiveTools` /
        // `tool_search` override installed since the session was built.
        // The hidden declarations are re-derived from the LIVE snapshot like
        // the build does, so a `/reload` keeps `codemode.mode: "only"` out of
        // the `<tools>`/`<rules>` sections.
        let (registry, active) = self.active_tool_snapshot(self.plugins.as_deref());
        let prompt_tools = declared_tool_infos(registry.as_ref(), &active);
        let hidden_tools = rust_hidden_declarations(registry.as_ref(), &active);
        // Ambient inputs: `refresh_context_inputs` re-reads the context FILES
        // (a reload is a disk refresh), but both paths keep the session's
        // BUILD-TIME identity — `self.agent_dir` and the once-resolved
        // `self.package_dir`. Calling the ambient wrapper here would
        // re-resolve `PIR_AGENT_DIR`/`PI_PACKAGE_DIR` and let a mid-process
        // change split the prompt off the session (D1).
        let mut prompt_context = self
            .prompt_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cache) = prompt_context.as_mut() else {
            return;
        };
        if refresh_context_inputs {
            cache.inputs.files = crate::coding_agent::context::resolve_context_files_with(
                &self.cwd,
                &crate::coding_agent::context::ContextOptions {
                    // The `--no-context-files` flag is a process-wide toggle,
                    // not session identity: like upstream's reload
                    // re-discovery, it is read fresh.
                    disabled: !crate::coding_agent::context::context_files_enabled(),
                    agent_dir: &self.agent_dir,
                    package_dir: self.package_dir.as_deref(),
                },
            );
        }
        cache.inputs.skills = skills;
        cache.inputs.mcp_servers = mcp_servers;
        // The reload is the new baseline for later active-set rebuilds: its
        // `(active, hidden)` snapshot and discovery inputs replace the
        // build-time ones. P7: the rebuild ALWAYS reinstalls the prompt and
        // its cache (never `None`), so a reload on a bare workspace cannot
        // silently drop the base prompt the build installed.
        let rebuilt = if cache.preamble_override.is_some() || !cache.append_sections.is_empty() {
            crate::coding_agent::context::session_system_prompt_from_inputs_with_overrides(
                &self.cwd,
                &prompt_tools,
                &hidden_tools,
                &cache.inputs,
                self.package_dir.as_deref(),
                cache.preamble_override.as_deref(),
                &cache.append_sections,
            )
        } else {
            crate::coding_agent::context::session_system_prompt_from_inputs(
                &self.cwd,
                &prompt_tools,
                &hidden_tools,
                &cache.inputs,
                self.package_dir.as_deref(),
            )
        };
        let rebuilt = with_prompt_addendum(rebuilt, cache.append.as_deref());
        cache.active = active;
        cache.hidden = hidden_tools.into_iter().collect();
        drop(prompt_context);
        *self
            .system_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rebuilt);
    }

    /// Extend the CONTEXT-DERIVED system prompt with caller-supplied addendum
    /// text (upstream `appendSystemPrompt`, rendered as the `addendum` section
    /// after the default prompt — `system-prompt.ts:172`).
    ///
    /// The SDK builder calls this for `.append_system_prompt(..)` WITHOUT
    /// `.system_prompt(..)`: that text must not replace the derived prompt
    /// (only an explicit `system_prompt` does that), so it is applied to the
    /// already-built prompt and recorded on the prompt cache. Every later
    /// rebuild (`/reload`, an active-set change, a skills install) then
    /// re-applies it as the last section.
    ///
    /// A no-op for an explicitly configured prompt (the caller owns that
    /// string; the builder itself appends to it) and for empty text.
    #[cfg(feature = "sdk")]
    pub(crate) fn append_derived_system_prompt(&self, appendix: impl Into<String>) {
        let appendix = appendix.into();
        if appendix.is_empty() || !self.context_derived_prompt {
            return;
        }
        let mut prompt_context = self
            .prompt_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cache) = prompt_context.as_mut() else {
            return;
        };
        cache.append = Some(match cache.append.take() {
            Some(existing) => format!("{existing}\n\n{appendix}"),
            None => appendix.clone(),
        });
        drop(prompt_context);
        let mut prompt = self
            .system_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(prompt) = prompt.as_mut() {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(&appendix);
        }
    }

    /// Apply the CLI prompt flags (`--system-prompt` /
    /// `--append-system-prompt`) to the structured prompt.
    ///
    /// Unlike [`Self::append_derived_system_prompt`], this keeps every
    /// structured section: the preamble is replaced and the append entries
    /// become the `addendum` section, exactly like upstream
    /// `buildSystemPrompt`'s `systemPrompt`/`appendSystemPrompt` options.
    /// Both are recorded on the prompt cache, so `/reload` and every
    /// active-set rebuild re-apply them.
    ///
    /// A no-op for a session whose host supplied an explicit full
    /// `system_prompt` (that string is the caller's).
    pub fn apply_prompt_overrides(&self, preamble: Option<String>, append: Vec<String>) {
        if !self.context_derived_prompt || (preamble.is_none() && append.is_empty()) {
            return;
        }
        let (registry, active) = self.active_tool_snapshot(self.plugins.as_deref());
        let prompt_tools = declared_tool_infos(registry.as_ref(), &active);
        let hidden_tools = rust_hidden_declarations(registry.as_ref(), &active);
        let mut prompt_context = self
            .prompt_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cache) = prompt_context.as_mut() else {
            return;
        };
        cache.preamble_override = preamble;
        cache.append_sections = append;
        let rebuilt =
            crate::coding_agent::context::session_system_prompt_from_inputs_with_overrides(
                &self.cwd,
                &prompt_tools,
                &hidden_tools,
                &cache.inputs,
                self.package_dir.as_deref(),
                cache.preamble_override.as_deref(),
                &cache.append_sections,
            );
        let rebuilt = with_prompt_addendum(rebuilt, cache.append.as_deref());
        cache.active = active;
        cache.hidden = hidden_tools.into_iter().collect();
        drop(prompt_context);
        *self
            .system_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rebuilt);
    }

    /// Rebuild the context-derived system prompt when the DECLARED/hidden set
    /// a loadout just resolved differs from the set the current prompt was
    /// rendered from (D6).
    ///
    /// This is the trigger upstream implements by calling `_rebuildSystemPrompt`
    /// on every active-set change — `setActiveTools` (`agent-session.ts:1532-1536`),
    /// the MCP extension's `ensureDiscoveryActive`, and `tool_search`'s load
    /// (`:3331-3336`). The session runs it from
    /// [`AgentSession::build_tool_loadout`] because that is the ONE place each
    /// settle iteration resolves the same active/hidden set the request
    /// declares; the prompt therefore cannot drift from the declarations.
    ///
    /// Steady-state cost: one `RwLock` read plus a slice/set comparison per
    /// iteration — no disk IO. The rebuild (only on an actual change) reuses
    /// the cached [`crate::coding_agent::context::PromptContextInputs`],
    /// exactly like upstream re-renders
    /// from its loaded resource sets. Changes take effect on the NEXT run,
    /// matching upstream's "changes take effect on the next agent turn"
    /// (`agent-session.ts:1522-1524`).
    pub(super) fn sync_context_prompt(
        &self,
        active: &[String],
        hidden: &std::collections::BTreeSet<String>,
        declared: &[std::sync::Arc<dyn crate::agent::tool_execution::AgentTool>],
    ) {
        if !self.context_derived_prompt {
            return;
        }
        // The MCP section is re-read on every settle iteration (upstream's
        // `before_agent_start` re-renders it per prompt): cheap, synchronous
        // and IO-free — the manager's state mirror, not a connect. A
        // `pending` -> `ready` transition therefore lands on the NEXT prompt,
        // never mid-prompt, and the comparison below makes an unchanged
        // section free.
        let mcp_servers =
            crate::coding_agent::context::mcp_servers_prompt_section(self.plugins.as_deref());
        let mut cache = self
            .prompt_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cache) = cache.as_mut() else {
            return;
        };
        if cache.active == active
            && cache.hidden == *hidden
            && cache.inputs.mcp_servers == mcp_servers
        {
            return;
        }
        // Only an actual change pays for the `ToolInfo` projection and the
        // prompt render; the discovery inputs stay cached.
        let prompt_tools: Vec<crate::system_prompt::ToolInfo> = declared
            .iter()
            .map(|tool| crate::system_prompt::ToolInfo::from_agent_tool(tool.as_ref()))
            .collect();
        let hidden_names: Vec<String> = hidden.iter().cloned().collect();
        // Borrow the cached discovery inputs directly: the rebuild is a pure
        // re-render, never a clone of the (possibly large) context files.
        // P7: the SESSION rebuild always renders (the cache always backs a
        // prompt), so a bare-workspace session keeps following the declared
        // set instead of falling back to "no system prompt".
        // P7: the SESSION rebuild always renders (the cache always backs a
        // prompt), so a bare-workspace session keeps following the declared
        // set instead of falling back to "no system prompt".
        cache.inputs.mcp_servers = mcp_servers;
        let rebuilt = if cache.preamble_override.is_some() || !cache.append_sections.is_empty() {
            crate::coding_agent::context::session_system_prompt_from_inputs_with_overrides(
                &self.cwd,
                &prompt_tools,
                &hidden_names,
                &cache.inputs,
                self.package_dir.as_deref(),
                cache.preamble_override.as_deref(),
                &cache.append_sections,
            )
        } else {
            crate::coding_agent::context::session_system_prompt_from_inputs(
                &self.cwd,
                &prompt_tools,
                &hidden_names,
                &cache.inputs,
                self.package_dir.as_deref(),
            )
        };
        let rebuilt = with_prompt_addendum(rebuilt, cache.append.as_deref());
        cache.active = active.to_vec();
        cache.hidden = hidden.clone();
        *self
            .system_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rebuilt);
    }

    /// Expand prompt commands (`/skill:name`, `/template`) in submitted text
    /// (upstream `_expandSkillCommand` + `expandPromptTemplate`).
    pub(super) fn expand_prompt_commands(&self, text: &str) -> String {
        let skills = self
            .skills
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let prompt_templates = self
            .prompt_templates
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        crate::coding_agent::resources::expand_prompt_commands(text, &skills, &prompt_templates)
    }

    /// A clone of the current model (thread-safe snapshot).
    pub(super) fn current_model(&self) -> Model {
        self.model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The current thinking level (thread-safe snapshot).
    pub(super) fn current_thinking(&self) -> ThinkingLevel {
        *self
            .thinking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current ephemeral-history snapshot: an `Arc` clone (zero copy);
    /// writers replace the slot, so the returned snapshot never mutates.
    pub(super) fn history_snapshot(&self) -> HistorySnapshot {
        self.history
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    // ------------------------------------------------------------------
    // Inbox / abort controls
    // ------------------------------------------------------------------

    /// Queue onto the steering inbox (channel + pending count) — the one
    /// enqueue path every caller shares so [`Self::get_steering_messages`]
    /// stays consistent with delivery. Emits a `queue_update` session event
    /// with the mirrored queue snapshot (upstream `_emitQueueUpdate` fires
    /// from `_queueUserInput`).
    pub(super) fn enqueue_steering(&self, prompt: agent_loop::PromptMessage) {
        let text = prompt.text_content();
        self.pending_messages.fetch_add(1, Ordering::SeqCst);
        let snapshot = {
            let mut mirror = self
                .queue_mirror
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mirror.steering.push(text);
            (mirror.steering.clone(), mirror.follow_up.clone())
        };
        self.steering_inbox.send(prompt);
        self.emit_session_event(SessionEvent::QueueUpdate {
            steering: snapshot.0,
            follow_up: snapshot.1,
            delivered_steering: Vec::new(),
            delivered_follow_up: Vec::new(),
        });
    }

    /// Queue onto the follow-up inbox (see [`Self::enqueue_steering`]).
    pub(super) fn enqueue_follow_up(&self, prompt: agent_loop::PromptMessage) {
        let text = prompt.text_content();
        self.pending_messages.fetch_add(1, Ordering::SeqCst);
        let snapshot = {
            let mut mirror = self
                .queue_mirror
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mirror.follow_up.push(text);
            (mirror.steering.clone(), mirror.follow_up.clone())
        };
        self.follow_up_inbox.send(prompt);
        self.emit_session_event(SessionEvent::QueueUpdate {
            steering: snapshot.0,
            follow_up: snapshot.1,
            delivered_steering: Vec::new(),
            delivered_follow_up: Vec::new(),
        });
    }

    /// Queue a steering message. Delivered after the current assistant turn
    /// finishes executing its tool calls, before the next model request.
    /// Prompt commands (`/skill:name`, `/template`) are expanded before
    /// queueing (upstream `_queueUserInput`).
    ///
    /// Returns the REAL disposition (pinned upstream contract): `Handled`
    /// when the agent is idle (the message will be delivered immediately at
    /// the next poll), `Queued` when a run is streaming and the message waits
    /// in the inbox.
    pub fn steer(&self, text: &str) -> Disposition {
        let expanded = self.expand_prompt_commands(text);
        let streaming = self.is_streaming();
        self.enqueue_steering(agent_loop::PromptMessage::text(expanded));
        if streaming {
            Disposition::Queued
        } else {
            Disposition::Handled
        }
    }

    /// Queue a follow-up message. Delivered once the agent has otherwise
    /// finished. Prompt commands are expanded before queueing. Returns the
    /// REAL disposition like [`Self::steer`].
    pub fn follow_up(&self, text: &str) -> Disposition {
        let expanded = self.expand_prompt_commands(text);
        let streaming = self.is_streaming();
        self.enqueue_follow_up(agent_loop::PromptMessage::text(expanded));
        if streaming {
            Disposition::Queued
        } else {
            Disposition::Handled
        }
    }

    /// Queue a pre-built model-facing message as a steering message
    /// (upstream `AgentMessage[]` steering: text plus image content blocks
    /// ride one user message). No prompt-command expansion is applied — the
    /// caller already built the final message.
    pub fn steer_message(&self, message: Message) -> Disposition {
        let streaming = self.is_streaming();
        self.enqueue_steering(agent_loop::PromptMessage::message(message));
        if streaming {
            Disposition::Queued
        } else {
            Disposition::Handled
        }
    }

    /// [`Self::steer_message`] for the follow-up queue.
    pub fn follow_up_message(&self, message: Message) -> Disposition {
        let streaming = self.is_streaming();
        self.enqueue_follow_up(agent_loop::PromptMessage::message(message));
        if streaming {
            Disposition::Queued
        } else {
            Disposition::Handled
        }
    }

    /// Queue a custom (`role: "custom"`) message as steering input (upstream
    /// `AgentMessage[]` supports custom messages). The model sees the same
    /// user-shaped projection a custom entry projects to; the message
    /// lifecycle carries the custom wire shape and the custom entry is what
    /// session storage receives (instead of a regular user message entry).
    pub fn steer_custom(&self, message: CustomRunMessage) -> Disposition {
        let streaming = self.is_streaming();
        let prompt = self.custom_prompt_message(&message);
        self.enqueue_steering(prompt);
        if streaming {
            Disposition::Queued
        } else {
            Disposition::Handled
        }
    }

    /// [`Self::steer_custom`] for the follow-up queue.
    pub fn follow_up_custom(&self, message: CustomRunMessage) -> Disposition {
        let streaming = self.is_streaming();
        let prompt = self.custom_prompt_message(&message);
        self.enqueue_follow_up(prompt);
        if streaming {
            Disposition::Queued
        } else {
            Disposition::Handled
        }
    }

    /// Build the loop prompt for one custom message: the user-shaped
    /// projection plus the custom wire shape and (for a stored session) the
    /// custom-entry persistence sink.
    pub(super) fn custom_prompt_message(
        &self,
        message: &CustomRunMessage,
    ) -> agent_loop::PromptMessage {
        let wire = message.to_json();
        let projected = custom_run_user_message(message);
        match self.queued_custom_persist_fn(message.clone(), wire.clone()) {
            Some(persist) => agent_loop::PromptMessage::custom(projected, wire, persist),
            // Store-less host: the projected message goes through the normal
            // produced-message path and enters the in-memory transcript.
            None => agent_loop::PromptMessage {
                message: projected,
                wire: Some(wire),
                persist: None,
            },
        }
    }

    /// The steering messages queued but not yet delivered (upstream
    /// `getSteeringMessages`), in queue order. Peek semantics: the loop's
    /// poll owns delivery, so this does not drain. Returns the message texts
    /// (the historical String API); use [`Self::peek_steering_messages`] for
    /// the full messages.
    pub async fn get_steering_messages(&self) -> Vec<String> {
        self.peek_steering_messages()
            .await
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect()
    }

    /// The follow-up messages queued but not yet delivered (upstream
    /// `getFollowUpMessages`), in queue order. Peek semantics. Returns the
    /// message texts; see [`Self::peek_follow_up_messages`].
    pub async fn get_follow_up_messages(&self) -> Vec<String> {
        self.peek_follow_up_messages()
            .await
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect()
    }

    /// The queued steering messages in full (text, image, and custom wire
    /// shapes included).
    pub async fn peek_steering_messages(&self) -> Vec<agent_loop::PromptMessage> {
        self.steering_inbox.peek().await
    }

    /// The queued follow-up messages in full.
    pub async fn peek_follow_up_messages(&self) -> Vec<agent_loop::PromptMessage> {
        self.follow_up_inbox.peek().await
    }

    /// The detached background completions waiting to be injected at the
    /// next tool-batch boundary (the model-facing queue; never mixed with
    /// user follow-ups). Returns their texts.
    pub async fn get_background_follow_up_messages(&self) -> Vec<String> {
        self.background_follow_up_inbox
            .peek()
            .await
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect()
    }

    /// Install (or clear) the per-tool lifecycle hooks every subsequent
    /// prompt run uses (upstream `AgentLoopConfig.beforeToolCall` /
    /// `afterToolCall`).
    ///
    /// Timing is the scheduler's and matches upstream: `before` runs after
    /// argument validation (and after the extension `tool_call` face, which
    /// owns the in-place `input` rewrite) and before execution; it may block
    /// the call (with an optional reason and `terminate` hint). `after` runs
    /// after the tool and before the extension `tool_result` face and the
    /// call's `tool_execution_end`; it may replace content/details/isError/
    /// usage/terminate. The extension faces still run exactly once — installing
    /// hooks never re-dispatches them. A change applies to runs started after
    /// it.
    pub fn set_tool_hooks(&self, hooks: Option<Arc<crate::agent::tool_execution::ToolHooks>>) {
        *self
            .tool_hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hooks;
    }

    /// The installed per-tool lifecycle hooks, if any.
    pub fn tool_hooks(&self) -> Option<Arc<crate::agent::tool_execution::ToolHooks>> {
        self.tool_hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Abort the in-flight run or compaction (no-op between operations; the
    /// next run gets a fresh signal). A running `/tree` branch summarization
    /// is cancelled too (upstream `abort()` → `abortBranchSummary()`,
    /// agent-session.ts:2465).
    pub fn abort(&self) {
        // In-flight extension tool calls observe the abort through the
        // runtime-level flag (the JS `signal` reads it); the next run clears
        // it again.
        if let Some(plugins) = &self.plugins {
            plugins.abort_current_call();
        }
        // Abort the signal FIRST, then re-read the session state. A run or
        // compaction start installs its signal after claiming the session;
        // an abort that overlaps that transition must reach the new signal,
        // and the post-abort state read records the abort request
        // (upstream `_agentRunAbortRequested`) whenever the owner became
        // visible before this point — the start's re-check then aborts its
        // fresh signal. Reading the state before aborting the signal could
        // miss an owner claimed in between and let the abort die on the
        // previous signal. `SESSION_COMPACTING` is included: the manual
        // compaction's summarizer stream carries this same session signal.
        self.signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort();
        let state = self.session_state();
        if state == SESSION_RUNNING || state == SESSION_COMPACTING {
            self.run_abort_requested.store(true, Ordering::SeqCst);
        }
        self.branch_summary_abort.cancel();
    }

    /// The live session claim ([`SESSION_IDLE`] / [`SESSION_RUNNING`] /
    /// [`SESSION_COMPACTING`]).
    pub(super) fn session_state(&self) -> u8 {
        self.session_state.load(Ordering::SeqCst)
    }

    /// Whether a prompt run is currently in flight.
    pub fn is_streaming(&self) -> bool {
        self.session_state() == SESSION_RUNNING
    }

    /// Whether this session was constructed as a parallel phone child
    /// (`PI_PHONE_CHILD=1` in the environment at construction). This is the
    /// cheap per-prompt guard for the inline image-token handling; parent
    /// sessions never pay more than one bool test.
    #[cfg(feature = "rpc")]
    pub(crate) fn is_phone_child(&self) -> bool {
        self.phone_child
    }

    /// Take the oldest custom-message run queued by `pi.sendMessage(...,
    /// {triggerTurn: true})` while idle (upstream: that message starts a run
    /// immediately). The facade has no idle loop, so a host drains this queue
    /// and runs each message through [`Self::run_custom_turn`] — while the
    /// session is busy the queue stays empty because `sendMessage` joins the
    /// run in flight instead.
    pub fn take_pending_custom_run(&self) -> Option<CustomRunMessage> {
        self.pending_custom_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    /// Whether a custom-message run is queued (see
    /// [`Self::take_pending_custom_run`]).
    pub fn has_pending_custom_runs(&self) -> bool {
        !self
            .pending_custom_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// The custom messages held for the next run's starting message
    /// (`deliverAs: "nextTurn"`), consumed by whichever run starts next.
    /// Peek semantics: the run consumes them (upstream
    /// `_pendingNextTurnMessages`).
    pub fn peek_pending_next_turn_messages(&self) -> Vec<CustomRunMessage> {
        self.pending_next_turn_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Consume the held `deliverAs: "nextTurn"` custom messages (upstream
    /// `_prepareRun` drains `_pendingNextTurnMessages` into the run it
    /// starts).
    pub(super) fn take_pending_next_turn_messages(&self) -> Vec<CustomRunMessage> {
        std::mem::take(
            &mut *self
                .pending_next_turn_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Consume the appended `triggerTurn: false` custom messages held for a
    /// store-less session (upstream `_appendCustomMessage` writes them into the
    /// session manager, which an ephemeral pir session does not have) so the run
    /// being built drains them into its first request. Always empty for a
    /// persisted session: those messages go straight into session storage.
    pub(super) fn take_pending_setup_messages(&self) -> Vec<CustomRunMessage> {
        std::mem::take(
            &mut *self
                .pending_setup_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// How an input handed to the session right now would be disposed
    /// (upstream `PromptDisposition`, the RPC `data.disposition` field):
    /// [`Disposition::Started`] when a prompt run can start immediately,
    /// [`Disposition::Queued`] when a run is in flight and the input queues
    /// behind it.
    pub fn input_disposition(&self) -> Disposition {
        if self.is_streaming() {
            Disposition::Queued
        } else {
            Disposition::Started
        }
    }

    /// Install a `prepare_request` hook (upstream `config.prepareRequest`).
    ///
    /// Runs before every conversational provider request (including the
    /// first); the returned update replaces the context, model, and/or
    /// thinking level for that request and later requests in the run.
    /// Pass `None` to remove a previously installed hook.
    pub fn set_prepare_request(&self, hook: Option<PrepareRequestHook>) {
        *self
            .prepare_request
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
    }

    /// Install a `finish_turn` hook (upstream `config.finishTurn`).
    ///
    /// Runs after each completed assistant turn and its tool results, before
    /// `turn_end`: `{action: "end"}` stops the run, `{action: "continue"}`
    /// guarantees exactly one more provider request. Pass `None` to remove a
    /// previously installed hook.
    pub fn set_finish_turn(&self, hook: Option<FinishTurnHook>) {
        *self
            .finish_turn
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
    }
}

/// The discovery inputs and declared/hidden snapshot the context-derived
/// system prompt was last built from.
///
/// Upstream keeps the resource loader's loaded context files and skills and
/// re-runs `_rebuildSystemPrompt` on every active-set change
/// (`agent-session.ts:1535`, `:3335`) — it never re-reads them for a tool
/// change. [`AgentSession::sync_context_prompt`] compares an incoming
/// loadout's `(active, hidden)` against this snapshot and only rebuilds when
/// they differ, so a steady turn does no IO and no prompt work (D6).
pub(super) struct PromptContextCache {
    /// The context files and skills the last render used.
    inputs: crate::coding_agent::context::PromptContextInputs,
    /// The declared names the prompt rendered (the loadout's `active`).
    active: Vec<String>,
    /// The hidden declarations the prompt was built with. `BTreeSet` because
    /// the loadout's hidden set is an order-independent set (Rust and JS
    /// `prepareLoadout` results unioned); the builder takes its iteration
    /// order.
    hidden: std::collections::BTreeSet<String>,
    /// The caller-supplied addendum appended after every render (upstream
    /// `appendSystemPrompt`, rendered as the `addendum` section,
    /// `system-prompt.ts:172`). Set by
    /// [`AgentSession::append_derived_system_prompt`]; `None` for an ordinary
    /// session. It lives on the cache — not in `PromptContextInputs` — because
    /// it is a caller prompt option, not a discovered resource.
    append: Option<String>,
    /// `--system-prompt`: the preamble override re-applied on every rebuild.
    preamble_override: Option<String>,
    /// `--append-system-prompt` entries, rendered as the `addendum` section by
    /// the prompt builder on every rebuild.
    append_sections: Vec<String>,
}

/// Append the caller-supplied addendum to a rendered derived prompt, separated
/// by the same blank line [`crate::system_prompt::build_system_prompt`] uses
/// between sections. Empty/absent text leaves the prompt untouched.
fn with_prompt_addendum(prompt: String, append: Option<&str>) -> String {
    match append {
        Some(text) if !text.is_empty() => format!("{prompt}\n\n{text}"),
        _ => prompt,
    }
}

/// The SYSTEM messages of the live projection, in projection order — the only
/// transcript input [`tools::restore_recorded_active_tools`] reads.
///
/// Built from [`SessionManager::projection_entry_meta`] (shared source entries
/// plus contributed message counts) instead of
/// [`SessionManager::build_context_entries`], so a resumed session no longer
/// materializes every stored assistant/tool-result message at construction:
/// the first prompt builds the projection the run actually consumes, and this
/// pass only clones the (few) system declarations. The newest compaction
/// contributes its stored checkpoint system message ahead of its summary (an
/// older compaction is stale and reports `message_count == 0`), matching
/// `session_entry_to_context_messages`.
fn session_recorded_system_messages(session: &SessionManager) -> Vec<Message> {
    let mut systems: Vec<Message> = Vec::new();
    if session.is_paged() {
        let _ = session.for_each_prefix_entry(|entry| {
            systems.extend(
                crate::session_manager::projection::session_entry_to_context_messages(&entry)
                    .into_iter()
                    .filter(|message| matches!(message, Message::System(_))),
            );
        });
    }
    for (index, meta) in session.projection_entry_meta().into_iter().enumerate() {
        if meta.message_count == 0 {
            continue;
        }
        match meta.source_entry.as_ref() {
            FileEntry::Message(inner) => match &inner.message {
                crate::session_manager::StoredMessage::System(_) => {
                    systems.push(inner.message.clone().into());
                }
                crate::session_manager::StoredMessage::User(user)
                    if user.role == crate::ai::types::UserRole::System =>
                {
                    systems.push(inner.message.clone().into());
                }
                _ => {}
            },
            FileEntry::Compaction(inner) if index == 0 => {
                if let Some(system) = &inner.system_message {
                    systems.push(system.to_message());
                }
            }
            _ => {}
        }
    }
    systems
}

/// The tool list the context-derived system prompt renders: the session's
/// DECLARED (active) tools, in declaration order — upstream
/// `_rebuildSystemPrompt`'s `selectedTools: validToolNames`
/// (agent-session.ts:1705-1731).
///
/// `hidden`-exposure tools never reach `registry` (the merged registry drops
/// them) and [`crate::agent::tool_execution::declared_tools`] guards them
/// again, so a registered-but-inactive tool (`codemode`/`deferred`, a
/// `default_active: false` core tool) is not advertised either. Rendering
/// exactly this set is what keeps the prompt's `<tools>`/`<rules>` sections
/// identical to the request's `Context.tools` / `loadout.definitions` (D6).
fn declared_tool_infos(
    registry: &crate::agent::tool_execution::ToolRegistry,
    active: &[String],
) -> Vec<crate::system_prompt::ToolInfo> {
    crate::agent::tool_execution::declared_tools(registry, active)
        .iter()
        .map(|tool| crate::system_prompt::ToolInfo::from_agent_tool(tool.as_ref()))
        .collect()
}

/// The hidden declarations the prompt's `hiddenTools` list carries: the union
/// of every DECLARED tool's Rust [`crate::agent::tool_execution::AgentTool::prepare_loadout`]
/// result (upstream `_rebuildSystemPrompt`'s `hiddenTools: [..._hiddenDeclarations]`,
/// agent-session.ts:1727, where `_hiddenDeclarations` is what
/// `_applyToolLoadout` collected, agent-session.ts:1589-1607).
///
/// This is the Rust half of [`AgentSession::build_tool_loadout`]'s hook
/// fan-out, run synchronously at build/reload time:
/// [`crate::agent::tool_execution::ToolLoadout`] needs the declared and
/// callable sets because a hook may classify by exposure
/// (`codemode` mode `"only"` hides the active `direct` tools that remain
/// callable). The hook is called for declared tools only, exactly as
/// `build_tool_loadout` and print do.
///
/// JS `prepareLoadout` hooks are async while session construction is
/// synchronous; they stay unrouted here and only affect the per-request
/// declarations (`build_tool_loadout`). The in-tree hook users that hide
/// declarations (`codemode`) and warm shells (`bash`/`powershell`) are Rust,
/// so the prompt's hidden set is complete for them.
fn rust_hidden_declarations(
    registry: &crate::agent::tool_execution::ToolRegistry,
    active: &[String],
) -> Vec<String> {
    let declared = crate::agent::tool_execution::declared_tools(registry, active);
    if declared.is_empty() {
        return Vec::new();
    }
    let callable = crate::agent::tool_execution::callable_tools(registry, active);
    let mut exposures = std::collections::BTreeMap::new();
    let mut namespaces = std::collections::BTreeMap::new();
    for tool in registry.iter() {
        exposures.insert(tool.name().to_owned(), tool.exposure());
        if let Some(namespace) = tool.namespace() {
            namespaces.insert(tool.name().to_owned(), namespace);
        }
    }
    let loadout = crate::agent::tool_execution::ToolLoadout {
        declared,
        callable,
        registered: registry.iter().cloned().collect(),
        exposures,
        namespaces,
    };
    let mut hidden = std::collections::BTreeSet::new();
    for tool in &loadout.declared {
        if let Some(changes) = tool.prepare_loadout(&loadout) {
            hidden.extend(changes.hidden_declarations);
        }
    }
    hidden.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Native phone host (the live-session side of `builtin:phone`)
// ---------------------------------------------------------------------------

/// Install the production phone adapter on the plugin host at construction.
///
/// The adapter (`LivePhoneHost`) stays unbound until
/// [`AgentSession::install_phone_host`] supplies the session `Arc`; the slot it
/// dispatches through is what makes the phone a mirror of this session instead
/// of the standalone placeholder surface.
#[cfg(feature = "phone")]
fn install_phone_plugin_adapter(host: &PluginHost) {
    if let Some(phone) = phone_plugin_from_host(host) {
        phone.ensure_live_session_slot();
    }
}

#[cfg(feature = "phone")]
fn phone_plugin_from_host(
    host: &PluginHost,
) -> Option<&crate::plugins::builtin::phone::PhonePlugin> {
    host.plugins().iter().find_map(|plugin| {
        plugin
            .as_any()?
            .downcast_ref::<crate::plugins::builtin::phone::PhonePlugin>()
    })
}

#[cfg(feature = "phone")]
fn phone_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl AgentSession {
    /// Build an `Arc`-wrapped session and immediately bind its live plugin
    /// services, so a native plugin's `on_context` sees a live session
    /// (`session_id`, model, thinking, ...) from its first activation.
    ///
    /// This is the constructor hosts should use instead of
    /// `Arc::new(AgentSession::new_with_plugins(..))`: binding needs the
    /// session `Arc`, which only exists after construction. Hosts that keep
    /// `new_with_plugins` must call [`AgentSession::install_phone_host`] once
    /// after wrapping.
    pub fn new_arc_with_plugins(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
    ) -> Arc<Self> {
        Self::new_arc_with_plugins_and_host(config, inline_plugins, None)
    }

    /// [`Self::new_arc_with_plugins`] bound to an explicit agent directory
    /// (see [`Self::new_with_agent_dir`]).
    pub fn new_arc_with_agent_dir(
        config: AgentSessionConfig,
        agent_dir: impl Into<PathBuf>,
    ) -> Arc<Self> {
        Self::new_arc_with_plugins_and_host_in(config, Vec::new(), None, Some(agent_dir.into()))
    }

    /// [`Self::new_arc_with_plugins`] around a host the caller already built
    /// with [`Self::build_plugin_host`].
    ///
    /// The SDK builder uses this to resolve its tool selection against the
    /// REAL merged registry (plugin/builtin tools registered exactly once)
    /// before the session is assembled. Passing `None` is identical to
    /// [`Self::new_arc_with_plugins`].
    pub(crate) fn new_arc_with_plugins_and_host(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        host: Option<Arc<PluginHost>>,
    ) -> Arc<Self> {
        Self::new_arc_with_plugins_and_host_in(config, inline_plugins, host, None)
    }

    /// Core arc constructor: `agent_dir` is the explicit override threaded into
    /// [`Self::build_with_live_services_and_host`] (`None` resolves the process
    /// environment once, at build).
    pub(crate) fn new_arc_with_plugins_and_host_in(
        config: AgentSessionConfig,
        inline_plugins: Vec<Arc<dyn crate::plugins::AgentPlugin>>,
        host: Option<Arc<PluginHost>>,
        agent_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let (session, services) =
            Self::build_with_live_services_and_host(config, inline_plugins, host, agent_dir);
        let session = Arc::new(session);
        // A `&self` operation must be able to reach the `Arc` to re-key the
        // native host registry after a storage swap.
        *session
            .self_weak
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(&session));
        crate::agent_session::native_host::register_native_session_host(&session);
        if let (Some(plugins), Some(services)) = (session.plugins.as_ref(), services) {
            // Fill the weak session slot BEFORE the bind: `on_context` is the
            // first place a plugin can read, and it must already see the
            // session.
            services.bind_session(&session);
            plugins.bind_context(services.into_context());
        }
        session.install_phone_host();
        session
    }

    /// Convenience twin of [`AgentSession::new_arc_with_plugins`] with no
    /// inline plugins.
    pub fn new_arc(config: AgentSessionConfig) -> Arc<Self> {
        Self::new_arc_with_plugins(config, Vec::new())
    }

    #[cfg(feature = "phone")]
    pub(crate) fn phone_plugin(&self) -> Option<&crate::plugins::builtin::phone::PhonePlugin> {
        phone_plugin_from_host(self.plugins.as_deref()?)
    }

    /// Whether this session's plugin host contains the phone builtin (the
    /// adapter may still be unbound until [`Self::install_phone_host`]).
    pub fn has_phone_plugin(&self) -> bool {
        #[cfg(feature = "phone")]
        {
            self.phone_plugin().is_some()
        }
        #[cfg(not(feature = "phone"))]
        {
            false
        }
    }

    /// Bind this session to the phone plugin's live adapter.
    ///
    /// Idempotent. Call once after the session is wrapped in `Arc`
    /// ([`Self::new_arc_with_plugins`] does it automatically). No-op when the
    /// phone builtin is not part of the plugin plan.
    ///
    /// Event delivery (agent/tool/queue envelopes) is owned by the session's
    /// native event lane; this method only wires commands and snapshot
    /// publication.
    pub fn install_phone_host(self: &Arc<Self>) {
        #[cfg(feature = "phone")]
        {
            let Some(plugins) = self.plugins.as_ref().cloned() else {
                return;
            };
            let Some(phone) = phone_plugin_from_host(&plugins) else {
                return;
            };
            phone.bind_live_session(Arc::downgrade(self));
            // Seed the host half of the health/status surface right away.
            // Installation is sync (hosts call it from a sync constructor), so
            // the first publication runs on the current runtime.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let session = Arc::clone(self);
                handle.spawn(async move {
                    session.publish_phone_state_now().await;
                });
            }
        }
    }

    /// Whether the phone plugin exists and is bound to this session.
    pub fn phone_host_bound(&self) -> bool {
        #[cfg(feature = "phone")]
        {
            self.phone_plugin()
                .is_some_and(|phone| phone.has_live_session())
        }
        #[cfg(not(feature = "phone"))]
        {
            false
        }
    }

    /// Publish the full live mirror at the run-settle boundary: status,
    /// catalog, session list and snapshot.
    ///
    /// A settled run that the phone did not start hands ownership back to the
    /// CLI (the plugin keeps `phone` while a phone-initiated run is still
    /// active, so its own settle stays `phone`). Called from the prompt
    /// run boundary only; command handlers use
    /// [`Self::publish_phone_state_now`].
    pub(crate) async fn publish_phone_state(&self) {
        #[cfg(feature = "phone")]
        {
            if let Some(phone) = self.phone_plugin() {
                phone.note_run_settled();
            }
            self.publish_phone_state_now().await;
        }
    }

    /// Publish the full live mirror without touching ownership (command
    /// handlers, `/phone-start`, install seeding).
    #[cfg_attr(not(feature = "phone"), allow(dead_code))]
    pub(crate) async fn publish_phone_state_now(&self) {
        #[cfg(feature = "phone")]
        {
            // Status is cheap and keeps the host half of `/api/health` fresh
            // even while the listener is stopped.
            self.publish_phone_status().await;
            let Some(phone) = self.phone_plugin() else {
                return;
            };
            // The snapshot/catalog/session-list build is expensive (transcript,
            // stats, session-directory scan). While no phone client can
            // receive it, only seed it once; `/phone-start` and `refresh`
            // publish the full state when a bridge exists.
            if !phone.has_bridge() && phone.has_published_snapshot() {
                return;
            }
            self.publish_phone_catalog().await;
            let snapshot = self.phone_snapshot_json().await;
            let session_list = self.phone_session_list_json().await;
            if let Some(phone) = self.phone_plugin() {
                phone.publish_session_list(session_list);
                phone.publish_snapshot(snapshot);
            }
        }
    }

    /// The run boundary ownership update: a run the phone did not start hands
    /// control back to the CLI (the adapter marks phone-initiated runs first
    /// through `PhonePlugin::begin_phone_run`).
    pub(crate) fn note_phone_run_started(&self) {
        #[cfg(feature = "phone")]
        {
            if let Some(phone) = self.phone_plugin() {
                phone.note_run_started();
            }
        }
    }

    /// Publish only the status object (cheap: no transcript/stats scan).
    pub(crate) async fn publish_phone_status(&self) {
        #[cfg(feature = "phone")]
        {
            let status = self.phone_status_json().await;
            if let Some(phone) = self.phone_plugin() {
                phone.publish_status(status);
            }
        }
    }

    /// Publish the catalog (live session rows).
    #[cfg_attr(not(feature = "phone"), allow(dead_code))]
    pub(crate) async fn publish_phone_catalog(&self) {
        #[cfg(feature = "phone")]
        {
            let catalog = self.phone_catalog_json().await;
            if let Some(phone) = self.phone_plugin() {
                phone.publish_catalog(catalog);
            }
        }
    }

    /// Surface a spawned-run failure after its command response was already
    /// acknowledged (the bridge has no other open channel for it).
    #[cfg_attr(not(feature = "phone"), allow(dead_code))]
    pub(crate) async fn publish_phone_run_error(&self, message: &str) {
        #[cfg(feature = "phone")]
        {
            if let Some(phone) = self.phone_plugin() {
                phone.publish_client_error(message);
            }
            self.publish_phone_status().await;
        }
        #[cfg(not(feature = "phone"))]
        {
            let _ = message;
        }
    }

    #[cfg(feature = "phone")]
    async fn phone_status_json(&self) -> serde_json::Value {
        let model = self.current_model();
        let streaming = self.is_streaming();
        let session_name = self.session_name().await;
        let pending = self.phone_pending_messages().await;
        let session_file = self.session_path().map(|path| path.display().to_string());
        serde_json::json!({
            "cwd": self.cwd.display().to_string(),
            "sessionId": self.session_id(),
            "sessionPath": session_file,
            "sessionFile": self.session_path().map(|path| path.display().to_string()),
            "sessionName": session_name,
            "model": serde_json::to_value(&model).unwrap_or(serde_json::Value::Null),
            "provider": model.provider,
            "modelId": model.id,
            "thinkingLevel": self.current_thinking().wire_key(),
            "isStreaming": streaming,
            "isCompacting": self.is_compacting(),
            "pendingMessageCount": self.pending_message_count(),
            "pendingMessages": pending,
            "sessionCount": 1,
            "childRunning": false,
            "lastError": "",
            "lastActivityAt": phone_now_ms(),
            "piCommand": "live cli",
        })
    }

    #[cfg(feature = "phone")]
    async fn phone_pending_messages(&self) -> Vec<serde_json::Value> {
        let steering = self.get_steering_messages().await;
        let follow_up = self.get_follow_up_messages().await;
        let mut pending = Vec::with_capacity(steering.len() + follow_up.len());
        for (index, text) in steering.iter().enumerate() {
            pending.push(serde_json::json!({
                "id": format!("steer-{index}"),
                "text": text,
                "imageCount": 0,
                "kind": "steer",
                "timestamp": phone_now_ms(),
            }));
        }
        for (index, text) in follow_up.iter().enumerate() {
            pending.push(serde_json::json!({
                "id": format!("follow-up-{index}"),
                "text": text,
                "imageCount": 0,
                "kind": "followUp",
                "timestamp": phone_now_ms(),
            }));
        }
        pending
    }

    /// The live-mirror snapshot body (`state`/`messages`/`commands`/...).
    ///
    /// The transcript is sent whole: `SessionBridge` applies the configured
    /// snapshot window and adds `historyTruncated`/`totalMessageCount`, which
    /// is what the client's `load-earlier-messages` paging indexes into.
    #[cfg(feature = "phone")]
    async fn phone_snapshot_json(&self) -> serde_json::Value {
        let model = self.current_model();
        let messages = self.transcript_json().await;
        let total = messages.len();
        let stats = self.session_stats().await;
        let context_usage = self.context_usage().await;
        let session_name = self.session_name().await;
        let commands = self.phone_command_catalog();
        let session_file = self.session_path().map(|path| path.display().to_string());
        serde_json::json!({
            "state": {
                "model": serde_json::to_value(&model).unwrap_or(serde_json::Value::Null),
                "thinkingLevel": self.current_thinking().wire_key(),
                "isStreaming": self.is_streaming(),
                "isCompacting": self.is_compacting(),
                "sessionFile": session_file,
                "sessionId": self.session_id(),
                "sessionName": session_name,
                "messageCount": total,
                "pendingMessageCount": self.pending_message_count(),
                "contextUsage": serde_json::to_value(&context_usage)
                    .unwrap_or(serde_json::Value::Null),
                "usageTotals": {
                    "input": stats.tokens.input,
                    "output": stats.tokens.output,
                    "reasoning": 0,
                    "cacheRead": stats.tokens.cache_read,
                    "cacheWrite": stats.tokens.cache_write,
                },
            },
            "messages": messages,
            "liveAssistantMessage": serde_json::Value::Null,
            "liveTools": [],
            "commands": commands,
        })
    }

    #[cfg(feature = "phone")]
    async fn phone_catalog_json(&self) -> serde_json::Value {
        let model = self.current_model();
        let session_name = self.session_name().await;
        let label = session_name.unwrap_or_else(|| "Current CLI session".to_owned());
        serde_json::json!({
            "activeSessionId": "parent-session",
            "sessions": [{
                "id": "parent-session",
                "label": label,
                "kind": "parent",
                "sessionId": self.session_id(),
                "cwd": self.cwd.display().to_string(),
                "lastActivityAt": phone_now_ms(),
                "isStreaming": self.is_streaming(),
                "hasPendingUiRequest": false,
                "model": {
                    "id": model.id,
                    "name": model.name,
                    "provider": model.provider,
                },
            }],
        })
    }

    /// Saved-session listing for `phone_list_sessions` (and the host-published
    /// session-list mirror).
    #[cfg_attr(not(feature = "phone"), allow(dead_code))]
    pub(crate) async fn phone_session_list_json(&self) -> serde_json::Value {
        let Some((session_dir, cwd)) = self
            .with_session_manager(|manager| {
                (
                    manager.session_dir().to_path_buf(),
                    manager.cwd().to_owned(),
                )
            })
            .await
        else {
            return serde_json::json!({ "sessions": [], "subagentCount": 0 });
        };
        let summaries =
            crate::session_manager::SessionManager::list_sessions(&session_dir, Some(cwd.as_str()));
        let mut sessions = Vec::with_capacity(summaries.len());
        let mut subagent_count = 0usize;
        for summary in &summaries {
            let is_subagent = summary
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with("subagent-"));
            if is_subagent {
                subagent_count += 1;
            }
            let modified = summary
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            let size_bytes = std::fs::metadata(&summary.path)
                .ok()
                .map(|metadata| metadata.len());
            let mut row = serde_json::json!({
                "path": summary.path.display().to_string(),
                "id": summary.id,
                "cwd": summary.cwd,
                "name": summary.name,
                "parentSessionPath": summary.parent_session_path,
                "modified": modified,
                "messageCount": summary.message_count,
                "firstMessage": summary.first_message,
                "isSubagent": is_subagent,
            });
            if let Some(size_bytes) = size_bytes {
                row["sizeBytes"] = serde_json::json!(size_bytes);
            }
            sessions.push(row);
        }
        serde_json::json!({ "sessions": sessions, "subagentCount": subagent_count })
    }

    /// The slash-command catalog the phone client renders.
    #[cfg(feature = "phone")]
    pub(crate) fn phone_command_catalog(&self) -> Vec<serde_json::Value> {
        let mut catalog = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        if let Some(plugins) = &self.plugins {
            for command in plugins.registry().resolved_commands() {
                if seen.insert(command.def.name.clone()) {
                    catalog.push(serde_json::json!({
                        "name": command.def.name,
                        "description": command.def.description,
                        "source": "extension",
                    }));
                }
            }
        }
        if let Some(runtime) = &self.extensions {
            for command in runtime.command_defs() {
                if seen.insert(command.name.clone()) {
                    catalog.push(serde_json::json!({
                        "name": command.name,
                        "description": command.description,
                        "source": "extension",
                    }));
                }
            }
        }
        for skill in self
            .skills
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            catalog.push(serde_json::json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
            }));
        }
        for template in self
            .prompt_templates
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            catalog.push(serde_json::json!({
                "name": template.name,
                "description": template.description,
                "source": "prompt",
            }));
        }
        catalog
    }
}
