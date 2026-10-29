use super::*;

impl AgentSession {
    // ------------------------------------------------------------------
    // Model / thinking-level management (upstream AgentSession mutators)
    // ------------------------------------------------------------------

    /// Switch the model (upstream `setModel`): applies to subsequent runs and
    /// is recorded on the session branch when the model actually changes.
    ///
    /// The credential is deliberately NOT re-resolved here: every request
    /// resolves the key for the model it runs under
    /// (`AgentSession::request_api_key_for`), so a switch — including one
    /// replayed by a resumed session — picks up the new provider's credential,
    /// and a credential change made since the session was built is picked up
    /// too, with no cached value to invalidate.
    ///
    /// This is the `source: "set"` shorthand for
    /// [`Self::set_model_with_source`].
    pub async fn set_model(&self, model: Model) -> crate::Result<()> {
        self.set_model_with_source(model, "set").await
    }

    /// The ONE model mutator (upstream `setModel` / `_emitModelSelect`): the
    /// state change, the branch `model_change` entry and the extension
    /// `model_select` event all originate here, so every entry point (TUI
    /// `/model` and cycling, RPC `set_model`/`cycle_model`, the SDK, and
    /// `pi.setModel`) reports the same payload.
    ///
    /// `source` is the upstream `ModelSelectEvent.source`: `"set"` for an
    /// explicit switch, `"cycle"` for `cycleModel`, `"restore"` for a resumed
    /// session's recorded selection. The event carries the FULL previous and
    /// next [`Model`] objects (not ids) and is skipped when the model did not
    /// actually change (upstream `_emitModelSelect`'s `modelsAreEqual` guard).
    pub async fn set_model_with_source(&self, model: Model, source: &str) -> crate::Result<()> {
        let previous = self.current_model();
        *self
            .model
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = model.clone();
        if previous.provider != model.provider || previous.id != model.id {
            if let Some(session) = &self.session {
                let mut session = session.lock().await;
                let _ = session.append_model_change(&model.provider, &model.id);
            }
        }
        // Upstream `_emitModelSelect` returns early when the previous and next
        // models are equal (`modelsAreEqual`): no event for a no-op switch.
        if previous.provider == model.provider && previous.id == model.id {
            return Ok(());
        }
        if let Some(plugins) = &self.plugins {
            // `emit` reaches every native plugin regardless of its declared
            // interest (upstream's runner walks registered handlers); skip
            // the fan-out when nobody declared the event.
            if plugins.has_handlers("model_select") {
                emit_plugin_event(
                    plugins,
                    "model_select",
                    serde_json::json!({
                        "type": "model_select",
                        "model": model,
                        "previousModel": previous,
                        "source": source,
                    }),
                )
                .await;
            }
        }
        Ok(())
    }

    /// Restore a resumed session's recorded model (upstream
    /// `_emitModelSelect(..., "restore")`): the explicit `"restore"` source
    /// for [`Self::set_model_with_source`], so a host attaching a stored
    /// session can report why the model moved.
    pub async fn restore_model(&self, model: Model) -> crate::Result<()> {
        self.set_model_with_source(model, "restore").await
    }

    /// Every model the registry can serve (upstream
    /// `modelRuntime.getAvailableSnapshot()`).
    pub fn available_models(&self) -> Vec<Model> {
        self.registry.get_models()
    }

    /// Look up one available model by provider + id.
    pub fn find_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.registry.get_model(provider, model_id)
    }

    /// The provider registry every request streams through.
    ///
    /// A host that swaps session storage (`Agent::attach_session`, the server's
    /// lazy rebuild) restores the incoming branch's persisted virtual-model
    /// registrations into this overlay BEFORE it resolves the branch's recorded
    /// selection — see [`restore_virtual_models_from`].
    #[cfg(feature = "sdk")]
    pub(crate) fn provider_registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    /// Cycle to the next available model (upstream `cycleModel`). When a
    /// scoped-model list is set (`--models` / `/scoped-models`), cycling
    /// walks the scope (filtered to what the registry can serve) and
    /// reports `is_scoped: true`; otherwise it walks every available model.
    /// Returns `None` when fewer than two models are available in the
    /// effective set.
    pub async fn cycle_model(&self) -> crate::Result<Option<ModelCycle>> {
        let scoped = self.scoped_models();
        let available: Vec<(String, String)> = self
            .available_models()
            .iter()
            .map(|model| (model.provider.clone(), model.id.clone()))
            .collect();
        if !scoped.is_empty() {
            // Upstream `_cycleScopedModel`: filter to available models, bail
            // on a single-entry scope, step forward with wrap-around.
            let current = self.current_model();
            let scoped_refs: Vec<(String, String)> = scoped
                .iter()
                .map(|scoped| (scoped.model.provider.clone(), scoped.model.id.clone()))
                .collect();
            if let Some((provider, model_id)) = crate::agent_session::scoped_cycle_target(
                &scoped_refs,
                &available,
                (&current.provider, &current.id),
            ) {
                let Some(next) = self.find_model(&provider, &model_id) else {
                    return Ok(None);
                };
                self.set_model_with_source(next.clone(), "cycle").await?;
                return Ok(Some(ModelCycle {
                    model: next,
                    thinking_level: self.current_thinking(),
                    is_scoped: true,
                }));
            }
            return Ok(None);
        }
        let models = self.available_models();
        if models.len() < 2 {
            return Ok(None);
        }
        let current = self.current_model();
        let index = models
            .iter()
            .position(|model| model.provider == current.provider && model.id == current.id)
            .unwrap_or(0);
        let next = models[(index + 1) % models.len()].clone();
        self.set_model_with_source(next.clone(), "cycle").await?;
        Ok(Some(ModelCycle {
            model: next,
            thinking_level: self.current_thinking(),
            // No scope set (upstream `_cycleAvailableModel` → isScoped: false).
            is_scoped: false,
        }))
    }

    /// Replace the session-only model scope (upstream `setScopedModels`).
    /// An empty list removes the scope, so cycling spans every available
    /// model again.
    pub fn set_scoped_models(&self, models: Vec<crate::ai::providers::ScopedModel>) {
        *self
            .scoped_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = models;
    }

    /// A snapshot of the session-only model scope (upstream `scopedModels`).
    pub fn scoped_models(&self) -> Vec<crate::ai::providers::ScopedModel> {
        self.scoped_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set the thinking level (upstream `setThinkingLevel`), clamped to what
    /// the current model supports, and recorded on the branch when it
    /// changes.
    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        let available = self.available_thinking_levels();
        let effective = if available.contains(&level) {
            level
        } else {
            clamp_thinking_level(level, &available)
        };
        let previous = self.current_thinking();
        *self
            .thinking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effective;
        if effective != previous {
            if let Some(session) = &self.session {
                let mut session = session.lock().await;
                let _ = session.append_thinking_level_change(effective.wire_key());
            }
            // Upstream `thinking_level_changed` (agent-session.ts:2692) fires
            // only for an EFFECTIVE change, after the branch entry was
            // appended; the level is the wire `ThinkingLevel` key.
            self.emit_session_event(SessionEvent::ThinkingLevelChanged {
                level: effective.wire_key().to_string(),
            });
            // The extension face of the same mutation (upstream
            // `thinking_level_select`, agent-session.ts:2693-2697): the
            // effective level plus the level it replaced. Both faces are
            // emitted from this ONE mutator, so hosts, the TUI and
            // `pi.setThinkingLevel` report the identical change.
            if let Some(plugins) = &self.plugins {
                if plugins.has_handlers("thinking_level_select") {
                    emit_plugin_event(
                        plugins,
                        "thinking_level_select",
                        serde_json::json!({
                            "type": "thinking_level_select",
                            "level": effective.wire_key(),
                            "previousLevel": previous.wire_key(),
                        }),
                    )
                    .await;
                }
            }
        }
        effective
    }

    /// Cycle to the next supported thinking level (upstream
    /// `cycleThinkingLevel`); `None` when the model does not reason.
    pub async fn cycle_thinking_level(&self) -> Option<ThinkingLevel> {
        if !self.current_model().reasoning {
            return None;
        }
        let levels = self.available_thinking_levels();
        if levels.is_empty() {
            return None;
        }
        let current = self.current_thinking();
        let index = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        let next = levels[(index + 1) % levels.len()];
        self.set_thinking_level(next).await;
        Some(next)
    }

    /// The thinking levels the current model supports (upstream
    /// `getAvailableThinkingLevels`): `["off"]` for non-reasoning models, the
    /// extended ladder filtered by the model's thinking-level map otherwise.
    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        supported_thinking_levels(&self.current_model())
    }
}

/// Result of `cycle_model` (upstream `ModelCycleResult`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCycle {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub is_scoped: bool,
}
/// The extended thinking ladder (upstream `EXTENDED_THINKING_LEVELS`).
const EXTENDED_THINKING_LEVELS: [ThinkingLevel; 7] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];

/// The thinking levels a model supports (upstream
/// `getSupportedThinkingLevels`): `["off"]` when the model does not reason;
/// otherwise the extended ladder, minus levels the model's thinking-level map
/// maps to `null`, and minus `xhigh`/`max` when the map does not name them.
pub(crate) fn supported_thinking_levels(model: &Model) -> Vec<ThinkingLevel> {
    if !model.reasoning {
        return vec![ThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level.wire_key()));
            match mapped {
                // `null` in the map means "unsupported".
                Some(None) => false,
                // `xhigh`/`max` only count as supported when named.
                None => !matches!(level, ThinkingLevel::XHigh | ThinkingLevel::Max),
                Some(Some(_)) => true,
            }
        })
        .collect()
}

/// Clamp a requested level to the nearest supported one (upstream
/// `clampThinkingLevel`): the closest supported level at or above the request
/// wins, else the closest below, else the first supported level.
fn clamp_thinking_level(requested: ThinkingLevel, available: &[ThinkingLevel]) -> ThinkingLevel {
    if available.contains(&requested) {
        return requested;
    }
    let requested_index = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|level| *level == requested);
    let Some(requested_index) = requested_index else {
        return available.first().copied().unwrap_or(ThinkingLevel::Off);
    };
    for candidate in EXTENDED_THINKING_LEVELS.iter().skip(requested_index) {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS.iter().take(requested_index).rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available.first().copied().unwrap_or(ThinkingLevel::Off)
}

/// The starting thinking level for a fresh (non-restored) session, shared by
/// the interactive host and the non-interactive print host: the upstream
/// `createAgentSession` fallback chain (sdk.ts:250-278) — an explicit level
/// (CLI `--thinking` or a `model:level` pattern suffix) wins, then the
/// per-model `modelThinkingLevels` override, then `settings
/// .defaultThinkingLevel`, then the shared
/// [`DEFAULT_THINKING_LEVEL`](crate::ai::providers::resolution::DEFAULT_THINKING_LEVEL).
///
/// `off` in the settings default is treated as absent (the setting expresses a
/// starting effort, not "disable thinking"); an explicit `off` — CLI, pattern
/// suffix, or per-model override — still wins. This is the policy the TUI
/// host's `initial_interactive_thinking` applies (settings default → shared
/// default, `off` absent); it lives here because the print host must build
/// with `tui` compiled out (`--no-default-features`).
pub fn resolve_starting_thinking_level(
    explicit: Option<ThinkingLevel>,
    per_model: Option<ThinkingLevel>,
    default_thinking_level: Option<ThinkingLevel>,
) -> ThinkingLevel {
    explicit
        .or(per_model)
        .or(default_thinking_level.filter(|level| *level != ThinkingLevel::Off))
        .unwrap_or(crate::ai::providers::resolution::DEFAULT_THINKING_LEVEL)
}

/// Parse an upstream thinking-level string (`off|minimal|low|medium|high|
/// xhigh|max`) into a [`ThinkingLevel`].
pub(super) fn parse_thinking_level(level: &str) -> Option<ThinkingLevel> {
    match level {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

impl AgentSession {
    /// The model compaction thresholds are checked against (upstream checks
    /// the ROUTED model under a virtual selection): the physical model a
    /// virtual selection routes to, with its limit overrides applied. An
    /// unrouted virtual model falls back to its catalog entry (context window
    /// 0 disables threshold compaction, like upstream).
    pub fn threshold_model_for(&self, model: &Model) -> Model {
        if crate::ai::virtual_::is_virtual_model(model) {
            if let Some(registration) = self
                .registry
                .virtual_model_registry()
                .get(&model.provider, &model.id)
            {
                let physical = self
                    .registry
                    .get_physical_model(&registration.physical.provider, &registration.physical.id);
                return registration.catalog_model(physical.as_ref());
            }
        }
        model.clone()
    }
}

/// Next model in the scoped-model list (`/model` cycling): the available
/// scoped entries in order, walking forward with wrap-around. `None` when
/// fewer than two scoped entries are available.
pub fn scoped_cycle_target(
    scoped: &[(String, String)],
    available: &[(String, String)],
    current: (&str, &str),
) -> Option<(String, String)> {
    let in_scope: Vec<&(String, String)> = scoped
        .iter()
        .filter(|model| available.contains(model))
        .collect();
    if in_scope.len() < 2 {
        return None;
    }
    let current_index = in_scope
        .iter()
        .position(|(provider, id)| provider.as_str() == current.0 && id.as_str() == current.1)
        .unwrap_or(0);
    let next = in_scope[(current_index + 1) % in_scope.len()];
    Some(next.clone())
}
