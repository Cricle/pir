use super::*;

impl AgentSession {
    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// The registry used for a run: the base tools plus every tool in the
    /// unified plugin registry (Rust builtins first, then JS extensions;
    /// first registration per name wins across plugins).
    ///
    /// The cache key is the plugin host's definition generation
    /// ([`PluginHost::registry_generation`]) plus the `setActiveTools`
    /// override. The generation moves on every host registry rebuild, so a
    /// definition-only change (a kept name whose description, parameter
    /// schema, output schema or exposure changed) invalidates the cached
    /// registry instead of serving the stale `AgentTool` objects. A cache hit
    /// is one `u64` compare and one override read — no tool definitions are
    /// cloned and no schema is re-serialized.
    pub(super) fn merged_tool_registry(&self, plugins: &PluginHost) -> Arc<ToolRegistry> {
        self.merged_tool_registry_with_count(plugins).0
    }

    /// [`Self::merged_tool_registry`] plus the cumulative number of cache
    /// misses (the counter behind [`Self::merged_tool_registry_for_test`]).
    fn merged_tool_registry_with_count(&self, plugins: &PluginHost) -> (Arc<ToolRegistry>, u64) {
        let generation = plugins.registry_generation();
        // The registry is the FULL registered set (base + every plugin tool):
        // upstream `getAllTools()` keeps tools that are not currently active
        // reachable for `setActiveTools`, the `tool_search` builtin and the
        // MCP discovery activation (`ensureDiscoveryActive` inspects
        // `codemode`/`tool_search` before activating them). The host's
        // explicit `--tools` selection is an ACTIVE-set override, applied by
        // [`Self::resolve_active_override`], never a registry filter —
        // filtering here made a selected `codemode` server unreachable and
        // left the session with only the allowlisted names. A runtime
        // `setActiveTools` override is likewise applied to the active set,
        // but it also decides which late-merged plugin definitions are
        // visible (a removed tool must not be re-added by a later pass).
        //
        // The one exception is a default-tools session's direct MCP bridges
        // ([`Self::mcp_direct_tool_names`]): an extension's `setActiveTools`
        // snapshot lists the tools that extension knows about, so taking it
        // literally would prune the connected servers' `direct` tools out of
        // the run registry and every MCP call would fail with "Tool not
        // found". Those bridges activate on registration upstream and stay
        // registered here; an EXPLICIT `--tools`/`--no-tools` selection still
        // prunes exactly as chosen.
        let registry_filter = plugins.active_tools_override().map(|mut names| {
            for name in self.mcp_direct_tool_names(Some(plugins)) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            names
        });
        let mut cache = self
            .merged_extension_tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_generation, cached_override, builds, registry)) = cache.as_ref() {
            if *cached_generation == generation && *cached_override == registry_filter {
                return (registry.clone(), *builds);
            }
        }
        let previous_builds = cache.as_ref().map(|entry| entry.2).unwrap_or(0);
        let mut registry = (*self.tools).clone();
        crate::agent::tool_execution::merge_plugin_tools(
            &mut registry,
            plugins,
            registry_filter.as_deref(),
        );
        // `hidden` tools are registered but unreachable (upstream `hidden`
        // exposure): they never join a session registry, so neither the model
        // nor other tools can reach them.
        registry.remove_exposure(crate::agent::tool_execution::ToolExposure::Hidden);
        let registry = Arc::new(registry);
        let builds = previous_builds + 1;
        *cache = Some((generation, registry_filter, builds, registry.clone()));
        (registry, builds)
    }

    /// Test seam: the merged registry plus the cumulative cache-miss count.
    /// Two calls with an unchanged host generation and override return the
    /// same `Arc` and the same count.
    #[doc(hidden)]
    pub fn merged_tool_registry_for_test(&self, plugins: &PluginHost) -> (Arc<ToolRegistry>, u64) {
        self.merged_tool_registry_with_count(plugins)
    }

    /// The merged registry plus the active (declared) tool names the session
    /// resolves right now — the snapshot [`crate::sdk::Agent::tools`] reports
    /// as the loadout of the next run.
    ///
    /// `plugins` is `None` for a session without a plugin host: the base
    /// registry (hidden tools removed) is the whole registry then.
    #[doc(hidden)]
    pub fn active_tool_snapshot(
        &self,
        plugins: Option<&PluginHost>,
    ) -> (Arc<ToolRegistry>, Vec<String>) {
        let registry = match plugins {
            Some(plugins) => self.merged_tool_registry(plugins),
            None => {
                let mut registry = (*self.tools).clone();
                registry.remove_exposure(crate::agent::tool_execution::ToolExposure::Hidden);
                Arc::new(registry)
            }
        };
        let active = registry.active_names(self.resolve_active_override(plugins).as_deref());
        (registry, active)
    }

    /// Whether the session's active set derives from the DEFAULT tools instead
    /// of an explicit selection (upstream `AgentSessionConfig.usesDefaultTools`).
    ///
    /// `true` when no host-resolved selection exists at all, or when the
    /// selection was derived from the settings `defaultTools` list / the
    /// [`crate::config::settings::DEFAULT_TOOL_NAMES`] base with
    /// `+name`/`-name` entries (including the transcript-restored loadout of a
    /// resumed session). An explicit `--tools <names>` / `--no-tools` /
    /// `--no-builtin-tools` selection is `false` and stays authoritative: a
    /// plugin may never add tools the user did not select.
    pub(crate) fn uses_default_tools(&self) -> bool {
        self.tool_selection
            .as_ref()
            .is_none_or(|selection| selection.uses_default_tools)
    }

    /// The exclusions a default-tools selection carries (`--exclude-tools`, or
    /// `-name` entries of a `--tools` modifier list).
    ///
    /// An explicit exclusion outranks a plugin's registration-time activation:
    /// the `mcp` builtin never re-adds a name listed here, so `--exclude-tools
    /// mcp__docs__search` keeps working on a default-tools session. An entry
    /// may be a `*` PATTERN (upstream `_excludedTools` is a matcher): the
    /// resolver carries `--exclude-tools 'mcp__docs__*'` forward verbatim so a
    /// bridge discovered after the selection was resolved is excluded too.
    /// Consumers must match entries with
    /// [`crate::coding_agent::tool_selection::tool_excluded_by`], not `==`.
    /// Empty for an explicit selection (nothing may be added there at all).
    pub(crate) fn default_tool_exclusions(&self) -> Vec<String> {
        let Some(selection) = self.tool_selection.as_ref() else {
            return Vec::new();
        };
        if !selection.uses_default_tools {
            return Vec::new();
        }
        selection
            .modifiers
            .iter()
            .filter_map(|entry| entry.strip_prefix('-'))
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The direct-exposure MCP bridge names a **default-tools** session keeps
    /// active even when a JS `setActiveTools` snapshot does not name them.
    ///
    /// See [`mcp_direct_tool_names_for`].
    #[cfg(feature = "mcp")]
    pub(super) fn mcp_direct_tool_names(&self, plugins: Option<&PluginHost>) -> Vec<String> {
        mcp_direct_tool_names_for(
            self.uses_default_tools(),
            &self.default_tool_exclusions(),
            plugins,
        )
    }

    /// Non-MCP builds have no bridges to keep active.
    #[cfg(not(feature = "mcp"))]
    fn mcp_direct_tool_names(&self, _plugins: Option<&PluginHost>) -> Vec<String> {
        Vec::new()
    }

    /// The active-tool override currently in effect (upstream
    /// `pi.getActiveTools()`'s source), in the loadout's precedence order:
    ///
    /// 1. the session's override cell (the `tool_search` builtin's matches and
    ///    the MCP discovery activation, which live in
    ///    [`AgentSession::active_tools_override`]),
    /// 2. the JS runtime's `setActiveTools` store,
    /// 3. the host-resolved `--tools` / settings selection — for a resumed
    ///    session the loadout its transcript records, with the selection's
    ///    `+name`/`-name` entries applied
    ///    ([`restore_recorded_active_tools`]).
    ///
    /// `None` means no override: the exposure defaults decide the active set.
    pub(super) fn resolve_active_override(
        &self,
        plugins: Option<&PluginHost>,
    ) -> Option<Vec<String>> {
        self.active_tools_override
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .or_else(|| plugins.and_then(PluginHost::active_tools_override))
            .or_else(|| {
                self.tool_selection
                    .as_ref()
                    .map(|selection| selection.enabled.clone())
            })
            .map(|mut names| {
                // Direct MCP bridges stay active on a default-tools session:
                // whichever store won, an extension's snapshot must not drop
                // the servers' registration-activated tools. An explicit
                // selection already answers `empty` here.
                for name in self.mcp_direct_tool_names(plugins) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                names
            })
    }

    /// Build the model-facing tool loadout for one run (upstream
    /// `_applyToolLoadout` + `_getCallableTools` +
    /// `_installHiddenDeclarationsProjection`).
    ///
    /// - The registry (hidden tools removed) stays the executable set:
    ///   `codemode`/`deferred` tools are callable without being declared.
    /// - The active set (the `setActiveTools` override, or every tool whose
    ///   exposure activates on registration) is declared to the model.
    /// - `prepareLoadout` hooks (Rust [`AgentTool::prepare_loadout`] plus the
    ///   JS runtime's hooks) may rewrite declared descriptions and hide
    ///   declarations (which stay active and callable).
    pub(super) async fn build_tool_loadout(
        &self,
        plugins: Option<&PluginHost>,
    ) -> SessionToolLoadout {
        let loadout_started = crate::config::timings::start_phase();
        use crate::agent::tool_execution::{
            callable_tools, declared_tool_definitions, declared_tools, ToolLoadout,
        };
        let registry = match plugins {
            Some(plugins) => self.merged_tool_registry(plugins),
            None => {
                let mut registry = (*self.tools).clone();
                registry.remove_exposure(crate::agent::tool_execution::ToolExposure::Hidden);
                Arc::new(registry)
            }
        };
        // A host-resolved tool selection IS the active override (upstream
        // creates exactly the selected tools); an extension `setActiveTools`
        // override wins over it.
        let override_names = self.resolve_active_override(plugins);
        let active = registry.active_names(override_names.as_deref());
        let declared = declared_tools(&registry, &active);
        let callable = callable_tools(&registry, &active);

        let mut exposures = std::collections::BTreeMap::new();
        let mut namespaces = std::collections::BTreeMap::new();
        for tool in registry.iter() {
            exposures.insert(tool.name().to_owned(), tool.exposure());
            if let Some(namespace) = tool.namespace() {
                namespaces.insert(tool.name().to_owned(), namespace);
            }
        }
        let loadout = ToolLoadout {
            declared: declared.clone(),
            callable: callable.clone(),
            registered: registry.iter().cloned().collect(),
            exposures,
            namespaces,
        };

        // Loadout hooks: Rust tools run theirs directly; JS tools (flagged in
        // their ToolDef) run through the QuickJS runtime.
        let mut descriptions = std::collections::BTreeMap::new();
        let mut hidden_declarations = std::collections::BTreeSet::new();
        for tool in &declared {
            if let Some(changes) = tool.prepare_loadout(&loadout) {
                descriptions.extend(changes.descriptions);
                hidden_declarations.extend(changes.hidden_declarations);
            }
        }
        if let Some((runtime, js_hooks)) = js_loadout_hooks(plugins) {
            let wire = loadout.to_wire_json();
            for tool in &declared {
                if !js_hooks.contains(tool.name()) {
                    continue;
                }
                match runtime.call_prepare_loadout(tool.name(), &wire).await {
                    Ok(Some(changes)) => {
                        if let Some(map) =
                            changes.get("descriptions").and_then(JsonValue::as_object)
                        {
                            for (name, description) in map {
                                if let Some(description) = description.as_str() {
                                    descriptions.insert(name.clone(), description.to_owned());
                                }
                            }
                        }
                        if let Some(names) = changes
                            .get("hiddenDeclarations")
                            .and_then(JsonValue::as_array)
                        {
                            for name in names {
                                if let Some(name) = name.as_str() {
                                    hidden_declarations.insert(name.to_owned());
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        crate::ext_diag!("[ext] prepareLoadout for {}: {error}", tool.name())
                    }
                }
            }
        }

        // D6: the system prompt follows the SAME declared/hidden set this
        // loadout just resolved. Upstream re-runs `_rebuildSystemPrompt` on
        // every active-set change (setActiveTools / tool_search / MCP
        // discovery); doing it here means every writer of the active override
        // is covered without a shared prompt handle, and the comparison makes
        // a steady turn a no-op (no IO: the context files/skills are cached).
        self.sync_context_prompt(&active, &hidden_declarations, &declared);

        let definitions = declared_tool_definitions(&declared, &descriptions, &hidden_declarations);
        let nested_callable = Arc::new(
            callable
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect::<std::collections::BTreeSet<String>>(),
        );
        let loadout = SessionToolLoadout {
            registry,
            definitions,
            nested_callable,
        };
        // Registry merge + `prepareLoadout` hooks (Rust and JS), once per
        // process: the first prompt's loadout build is the one packages can
        // make expensive, later builds reuse their own warmed state.
        crate::config::timings::log_phase_once_opt(
            "loadout.first_build",
            "loadout.first_build",
            crate::config::timings::finish_phase(loadout_started),
        );
        loadout
    }

    /// Live tool partial-result channel for the run: when inline Rust plugins
    /// are attached (SDK embedders), updates dispatch through the unified
    /// plugin host so Rust plugins observe them (the JS adapter rides the
    /// same dispatch); otherwise the direct extension-runtime path is kept.
    /// Register a host-facing live partial-result sink (the TUI's streaming
    /// tool blocks). Independent of extension runtimes: the sink receives
    /// every call-id-carrying update the tools emit.
    pub fn set_host_tool_update(&self, sink: crate::agent::tool_execution::ToolUpdateFn) {
        *self
            .host_tool_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    pub(super) fn tool_update_channel(&self) -> Option<crate::agent::tool_execution::ToolUpdateFn> {
        // The host sink always wins when registered (TUI streaming), with
        // the plugin/extension dispatch chained after it.
        if let Some(sink) = self
            .host_tool_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let plugins = self.plugins.clone();
            let native = plugins
                .as_ref()
                .is_some_and(|plugins| plugins.has_native_handlers("tool_execution_update"));
            let extensions = self.extensions.clone();
            let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(EXT_DISPATCH_CONCURRENCY));
            return Some(Arc::new(move |partial: JsonValue| {
                // The host sink needs its own snapshot; the extension payload
                // then consumes the original instead of cloning it again.
                sink(partial.clone());
                // Upstream payload parity: the event carries the call identity
                // alongside the partial result, not the bare snapshot.
                let payload = crate::agent::tool_execution::tool_update_payload_owned(partial);
                // Native fan-out only (`emit_native` skips the JS adapter):
                // the direct runtime below is the JS face, so a session with
                // both runtimes dispatches each face exactly once.
                if native {
                    if let Some(plugins) = plugins.clone() {
                        let mut payload = payload.clone();
                        spawn_ext_dispatch(&dispatch_sem, async move {
                            plugins
                                .emit_native("tool_execution_update", &mut payload)
                                .await;
                        });
                    }
                }
                if let Some(runtime) = extensions.clone() {
                    let mut payload = payload;
                    spawn_ext_dispatch(&dispatch_sem, async move {
                        if runtime.has_handlers("tool_execution_update") {
                            if let Err(error) =
                                runtime.emit("tool_execution_update", &mut payload).await
                            {
                                crate::ext_diag!(
                                    "[ext] tool_execution_update dispatch failed: {error}"
                                );
                            }
                        }
                    });
                }
            }));
        }
        if self.has_inline_plugins {
            let plugins = self.plugins.as_ref()?.clone();
            let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(EXT_DISPATCH_CONCURRENCY));
            return Some(Arc::new(move |partial: JsonValue| {
                // Fire-and-forget live dispatch (the JS prelude's `onUpdate`
                // path queues the same event shape).
                let sem = Arc::clone(&dispatch_sem);
                let plugins = plugins.clone();
                spawn_ext_dispatch(&sem, async move {
                    let mut payload =
                        crate::agent::tool_execution::tool_update_payload_owned(partial);
                    if let Err(error) = plugins.emit("tool_execution_update", &mut payload).await {
                        crate::ext_diag!("[ext] tool_execution_update dispatch failed: {error}");
                    }
                });
            }));
        }
        // Builtin-only host (plan-only extension handle, no inline plugins):
        // the host still owns Rust builtins that declared
        // `tool_execution_update` interest (the phone bridge's heartbeat
        // path). Dispatch to them with `emit_native` and keep the JS face on
        // its direct runtime handle — exactly once each, fire-and-forget under
        // the same concurrency/time bounds as the other channels.
        if let Some(plugins) = self.plugins.as_ref() {
            if plugins.has_native_handlers("tool_execution_update") {
                let plugins = Arc::clone(plugins);
                let extensions = self.extensions.clone();
                let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(EXT_DISPATCH_CONCURRENCY));
                return Some(Arc::new(move |partial: JsonValue| {
                    let payload = crate::agent::tool_execution::tool_update_payload_owned(partial);
                    let plugins = plugins.clone();
                    let mut native_payload = payload.clone();
                    spawn_ext_dispatch(&dispatch_sem, async move {
                        plugins
                            .emit_native("tool_execution_update", &mut native_payload)
                            .await;
                    });
                    if let Some(runtime) = extensions.clone() {
                        let mut js_payload = payload;
                        spawn_ext_dispatch(&dispatch_sem, async move {
                            if runtime.has_handlers("tool_execution_update") {
                                if let Err(error) =
                                    runtime.emit("tool_execution_update", &mut js_payload).await
                                {
                                    crate::ext_diag!(
                                        "[ext] tool_execution_update dispatch failed: {error}"
                                    );
                                }
                            }
                        });
                    }
                }));
            }
        }
        extension_tool_update_channel(self.extensions.as_ref())
    }
}

// ---------------------------------------------------------------------------
// Direct MCP bridges (registration-time activation for default-tools sessions)
// ---------------------------------------------------------------------------

/// The direct-exposure MCP bridge names a **default-tools** session keeps
/// active even when a JS `setActiveTools` snapshot does not name them.
///
/// Upstream activates a `direct` tool when it registers; pir's active set is
/// an override list, so an extension's `setActiveTools` snapshot (an extension
/// reload publishing its own tool list) otherwise removed every connected
/// server's bridges — the run registry pruned them
/// ([`AgentSession::merged_tool_registry`]) and calls failed with "Tool not
/// found" until something rewrote the store.
///
/// Empty when the session has an explicit `--tools`/`--no-tools` selection
/// (the user's set is authoritative) or when no `mcp` builtin is loaded.
/// Names excluded through `-name` or `-pattern` entries are never returned
/// (upstream `_excludedTools` is a matcher), and the exposure is resolved
/// from the server's LIVE config so a `/mcp disable`/exposure change takes
/// effect before the next registry rebuild hides/re-registers the bridge.
///
/// Shared by the session's own resolution and the plugin-service adapter's
/// `active_tools()` copy of the precedence chain, so the two faces of "the
/// effective active set" cannot disagree.
#[cfg(feature = "mcp")]
pub(crate) fn mcp_direct_tool_names_for(
    uses_default_tools: bool,
    exclusions: &[String],
    plugins: Option<&PluginHost>,
) -> Vec<String> {
    if !uses_default_tools {
        return Vec::new();
    }
    let Some(plugins) = plugins else {
        return Vec::new();
    };
    // Cheap gate: a host without the `mcp` builtin has no bridges at all, and
    // this runs on every active-set resolution.
    let Some(manager) = plugins.mcp_manager() else {
        return Vec::new();
    };
    let registry = plugins.registry();
    let registered = registry.resolved_tools();
    let registered_names: std::collections::BTreeSet<&str> = registered
        .iter()
        .map(|(def, _)| def.name.as_str())
        .collect();
    let registered_direct: std::collections::BTreeSet<&str> = registered
        .iter()
        .filter(|(def, _)| def.exposure == "direct")
        .map(|(def, _)| def.name.as_str())
        .collect();
    // A bridge is unwanted when its server is disabled, its exposure no
    // longer resolves to `direct`, or a default-tools exclusion matches it.
    //
    // The exposure is resolved from the LIVE entry, not the registered
    // definition: `/mcp disable` and `/mcp setExposure` update the entry and
    // recompute before the next registry rebuild re-registers the bridge
    // (upstream `hideTools` / `setExposure`), and this union runs on every
    // active-set resolution in between. The file-level `builtinTools` default
    // is the plugin's registration decision, so an entry that leaves
    // `exposure` unset falls back to the REGISTERED definition's resolution.
    let discovered = manager.discovered_tools();
    let mut wanted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in manager.registry().list() {
        if !entry.config.is_enabled() {
            continue;
        }
        let Some(tools) = discovered.get(&entry.name) else {
            // No discovery snapshot: a bridge registered for this server by
            // another registration path (an extension's `registerTool`, the
            // unit fixtures). Its registered `direct` exposure is the only
            // exposure fact the manager can offer; keep the pre-snapshot union
            // behavior for those names.
            let namespace = crate::mcp::registry::mcp_namespace(&entry.name);
            for name in &registered_direct {
                if name
                    .strip_prefix(namespace.as_str())
                    .is_some_and(|rest| rest.starts_with("__"))
                {
                    wanted.insert((*name).to_owned());
                }
            }
            continue;
        };
        let tool_names: Vec<String> = tools.iter().map(|tool| tool.tool.name.clone()).collect();
        for (discovered, assigned) in tools
            .iter()
            .zip(crate::mcp::registry::assign_mcp_tool_names(
                &entry.name,
                &tool_names,
            ))
        {
            if !registered_names.contains(assigned.as_str()) {
                continue;
            }
            let direct = match entry
                .config
                .base()
                .explicit_tool_exposure(&discovered.tool.name)
            {
                Some(exposure) => exposure == crate::config::mcp::McpExposure::Direct,
                None => match entry.config.base().exposure {
                    Some(exposure) => exposure == crate::config::mcp::McpExposure::Direct,
                    None => registered_direct.contains(assigned.as_str()),
                },
            };
            if direct {
                wanted.insert(assigned);
            }
        }
    }
    registered
        .into_iter()
        .filter(|(def, _)| wanted.contains(&def.name))
        .filter(|(def, _)| {
            !crate::coding_agent::tool_selection::tool_excluded_by(exclusions, &def.name)
        })
        .map(|(def, _)| def.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Recorded active tools (session resume, upstream `_restoreToolsFromTranscript`)
// ---------------------------------------------------------------------------

/// The active-tool loadout the session transcript records — the source
/// upstream `_restoreToolsFromTranscript` reads through
/// `getCurrentSystemMessage(...).toolsAdded` (agent-session.ts:1810-1825): the
/// accumulated `toolsAdded` minus `toolsRemoved` over the transcript's system
/// declaration messages, in transcript order (the same replay
/// `agent_loop::declared_tool_names` seeds `declare_tool_changes` with).
///
/// `None` when the transcript records no declaration at all — the session then
/// keeps the settings/CLI base — and also when the record is not COMPLETE.
/// pir's compaction entry stores only the system prompt text
/// ([`crate::session_manager::StoredSystemMessage`]), while upstream's stores
/// the whole current system message including its tools
/// (session-manager.ts:1270), so after a compaction the surviving declarations
/// are a delta of the pre-compaction loadout and restoring them could drop
/// tools. Only a record that opens with a declaration is complete, which is the
/// shape every pir transcript has (the first request of a session declares the
/// whole loadout).
fn recorded_tool_loadout(messages: &[Message]) -> Option<Vec<String>> {
    let mut declared: Option<Vec<String>> = None;
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        if system.tools_added.is_none() && system.tools_removed.is_none() {
            // A system message that declares nothing: the compaction replay of
            // the system prompt. It carries no tool information, and a record
            // that starts with one is incomplete (see above).
            declared.as_ref()?;
            continue;
        }
        let names = declared.get_or_insert_with(Vec::new);
        for tool in system.tools_added.iter().flatten() {
            if !names.iter().any(|name| name == &tool.name) {
                names.push(tool.name.clone());
            }
        }
        for tool in system.tools_removed.iter().flatten() {
            names.retain(|name| name != &tool.name);
        }
    }
    declared
}

/// Apply the transcript's recorded loadout to a session's base tool selection
/// (upstream `_restoreToolsFromTranscript`, reached from the constructor when
/// `usesDefaultTools`; 6f2f164ab, agent-session.ts:523-527): the recorded names
/// with the selection's `+name`/`-name` entries applied in list order become
/// the session's active set. The base selection's registry filter is left
/// untouched (upstream filters its registry by the allowlist/exclusions only,
/// never by `defaultTools`), so the restored names are the ones the loadout
/// activates.
///
/// The selection stays unchanged when it is explicit — `--tools <names>`,
/// `--no-tools` or `--no-builtin-tools` — or when the transcript records no
/// loadout: upstream's `usesDefaultTools` gate. A `None` selection (no
/// settings/CLI change) is a default-tools session and restores too.
pub(super) fn restore_recorded_active_tools(
    selection: Option<crate::coding_agent::ToolSelection>,
    history: &[Message],
) -> Option<crate::coding_agent::ToolSelection> {
    let uses_default_tools = match selection.as_ref() {
        Some(selection) => selection.uses_default_tools,
        None => true,
    };
    if !uses_default_tools {
        return selection;
    }
    let Some(recorded) = recorded_tool_loadout(history) else {
        return selection;
    };
    let modifiers: &[String] = selection
        .as_ref()
        .map(|selection| selection.modifiers.as_slice())
        .unwrap_or_default();
    // Exact `-name` entries apply in list order; `-pattern` entries are
    // matchers (the resolver carries an `--exclude-tools 'mcp__*'` forward for
    // late discovery), so they are applied to the recorded loadout after the
    // exact modifiers — a resumed session must exclude exactly the names a
    // fresh build excludes.
    let mut names = crate::config::settings::apply_tool_modifiers(&recorded, modifiers);
    let patterns: Vec<String> = modifiers
        .iter()
        .filter_map(|entry| entry.strip_prefix('-'))
        .filter(|pattern| pattern.contains('*'))
        .map(str::to_owned)
        .collect();
    if !patterns.is_empty() {
        names
            .retain(|name| !crate::coding_agent::tool_selection::tool_excluded_by(&patterns, name));
    }
    Some(crate::coding_agent::ToolSelection {
        enabled: names,
        // The `+name`/`-name` entries are applied above (the restored set is
        // the recorded loadout, not a modifier list over the defaults), but
        // the EXCLUSIONS are kept: they are the user's explicit "never this
        // tool" statement, and the `mcp` builtin reads them when it decides
        // whether a registration-time bridge may be re-activated
        // ([`AgentSession::default_tool_exclusions`]). Dropping them would let
        // a resumed session re-wake an MCP tool the previous run excluded.
        modifiers: modifiers
            .iter()
            .filter(|entry| entry.starts_with('-'))
            .cloned()
            .collect(),
        uses_default_tools: true,
    })
}

/// Live `tool_execution_update` channel for a bare extension runtime — a host
/// with no [`AgentSession`] (print mode). Same projection and dispatch policy
/// as [`AgentSession::tool_update_channel`]'s extension-runtime branch, which
/// now delegates here: the partial snapshot is projected by
/// [`crate::agent::tool_execution::tool_update_payload`] (call identity included) and dispatched
/// fire-and-forget under the shared concurrency/time bounds of
/// [`spawn_ext_dispatch`]. `None` when no runtime is attached.
///
/// Session-less hosts reach it through `LoopDeps::tool_updates`, which the
/// loop hands to every tool context as `on_update` (upstream
/// `emitToolExecutionUpdate`, agent-loop.ts:779-788).
pub(crate) fn extension_tool_update_channel(
    runtime: Option<&crate::extensions::ExtensionHandle>,
) -> Option<crate::agent::tool_execution::ToolUpdateFn> {
    let runtime = runtime?.clone();
    let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(EXT_DISPATCH_CONCURRENCY));
    Some(Arc::new(move |partial: JsonValue| {
        let runtime = runtime.clone();
        spawn_ext_dispatch(&dispatch_sem, async move {
            if runtime.has_handlers("tool_execution_update") {
                let mut payload = crate::agent::tool_execution::tool_update_payload_owned(partial);
                if let Err(error) = runtime.emit("tool_execution_update", &mut payload).await {
                    crate::ext_diag!("[ext] tool_execution_update dispatch failed: {error}");
                }
            }
        });
    }))
}

/// Bound on concurrently in-flight extension-dispatch tasks per tool-update
/// channel.
const EXT_DISPATCH_CONCURRENCY: usize = 8;

/// Wall-clock cap on one fire-and-forget extension dispatch of a
/// `tool_execution_update` snapshot.
const EXT_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Spawn one fire-and-forget extension dispatch for a tool-update channel.
///
/// Sound under any caller runtime, including the shell pool's dedicated
/// single-worker reactor (`ShellPool::execute_local` invokes the channel's
/// emitter from inside the pool runtime): the spawned task awaits the
/// extension actor's bounded mailbox (cap 64), so an UNBOUNDED task stream
/// fed by a slow JS handler would pile up on — and starve — the pool worker.
/// Two bounds prevent that:
///
/// - **Concurrency**: a semaphore permit is taken up-front and held by the
///   spawned task. When the channel is saturated (a slow handler keeping 8
///   dispatches in flight), the throttled streaming snapshot is DROPPED —
///   the same degradation the update throttle already applies, never a
///   growth path.
/// - **Time**: a dispatch running past [`EXT_DISPATCH_TIMEOUT`] is
///   abandoned, so tasks cannot outlive a cancelled call indefinitely
///   (mirrors the extension call timeout contract; the payload is
///   fire-and-forget, nobody joins it).
fn spawn_ext_dispatch<F>(semaphore: &Arc<tokio::sync::Semaphore>, dispatch: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let Ok(permit) = semaphore.clone().try_acquire_owned() else {
        // Saturated: drop this snapshot instead of queueing another task.
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        let _ = tokio::time::timeout(EXT_DISPATCH_TIMEOUT, dispatch).await;
    });
}

/// The tool loadout one prompt run is built from (upstream `_applyToolLoadout`
/// results): the executable registry, the model-facing tool definitions, the
/// `ctx.executeTool` callable set, and the hidden-declaration bookkeeping.
pub(crate) struct SessionToolLoadout {
    /// The executable registry: `codemode`/`deferred` tools stay callable
    /// without being declared; `hidden` tools are absent.
    pub registry: Arc<ToolRegistry>,
    /// Tool definitions declared to the model (active set after loadout
    /// hooks), passed as the request's tool list.
    pub definitions: Vec<crate::ai::types::Tool>,
    /// Names tools may call through `ctx.executeTool()` (upstream
    /// `_getCallableTools`).
    pub nested_callable: Arc<std::collections::BTreeSet<String>>,
}

/// The JS runtime plus the names of its tools that declare a
/// `prepareLoadout` hook (upstream `ToolDefinition.prepareLoadout`).
fn js_loadout_hooks(
    plugins: Option<&PluginHost>,
) -> Option<(
    crate::extensions::ExtensionHandle,
    std::collections::BTreeSet<String>,
)> {
    let runtime = plugins.and_then(PluginHost::js_handle)?.clone();
    let names = runtime
        .tool_defs()
        .into_iter()
        .filter(|def| def.prepare_loadout)
        .map(|def| def.name)
        .collect();
    Some((runtime, names))
}
