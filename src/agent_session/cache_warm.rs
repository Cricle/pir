use super::*;

impl AgentSession {
    // ------------------------------------------------------------------
    // Cache warming (src/cache_warmer.rs; minimal additive seam)
    // ------------------------------------------------------------------

    /// Fire at most one warm-up request for this settled run (upstream
    /// `CacheWarmer`). Gated on: warming installed and not `"off"`, a
    /// successful non-aborted response whose usage reports cacheable input,
    /// a replayable model (upstream `isReplayable`), and a non-empty replayed
    /// conversation. Both modes spawn a DETACHED bounded task (upstream arms
    /// an unref'd timer): prompt() never waits on the warm request. The task
    /// is cancelled by the next prompt, which aborts the live warm's
    /// await-able [`AbortSignal`] the moment the conversation goes stale. Warm
    /// requests go straight to the
    /// provider registry — they never re-enter the agent loop, so warming
    /// cannot recurse.
    /// [Narrowed to the terminal message] the settled run's produced messages
    /// have already moved into the persistence batch, so the warm gate's only
    /// run-result input — the terminal assistant message — is taken by borrow.
    pub(super) async fn maybe_warm_cache(
        &self,
        terminal: &AssistantMessage,
        signal: &AbortSignal,
        run_system_prompt: Option<String>,
    ) {
        let warmer = *self
            .cache_warming
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(warmer) = warmer else {
            return;
        };
        use crate::config::settings::CacheWarmingMode;
        if warmer.mode == CacheWarmingMode::Off {
            return;
        }
        if !crate::cache_warmer::should_warm(terminal, signal.is_aborted()) {
            return;
        }
        // Upstream `isReplayable`: a replay that would derive different
        // Anthropic `budget_tokens` would miss the cache key (and could still
        // think for thousands of tokens) — never warm those models.
        let model = self.current_model();
        if !crate::cache_warmer::is_replayable(&model, Some(self.current_thinking())) {
            return;
        }
        // cache_warming_decision (upstream `emitCacheWarmingDecision`): after
        // pi's own gate, before the refresh is sent, extensions may override
        // whether it is sent — the merged `{action}` result decides and the
        // last override wins ("stop" ends warming until the next real
        // request). Dispatch failures fall back to pi's decision (warm). The
        // event carries the same economics upstream evaluates
        // (cache-warmer.ts:112-115, 378-399): the price of this refresh
        // (`warmCost`), the extra price of the next real request if the cache
        // entry is lost (`missCost`), and the estimated chance a real request
        // arrives first (`continuationProbability`).
        if let Some(plugins) = &self.plugins {
            if plugins.has_handlers("cache_warming_decision") {
                let (warm_cost, miss_cost, continuation_probability) =
                    cache_warming_economics(&model, terminal, warmer.mode);
                let mut payload = serde_json::json!({
                    "type": "cache_warming_decision",
                    "warmCost": warm_cost,
                    "missCost": miss_cost,
                    "continuationProbability": continuation_probability,
                    "action": "warm",
                });
                match plugins.emit("cache_warming_decision", &mut payload).await {
                    Ok(outcome) => {
                        if outcome.result.get("action").and_then(JsonValue::as_str) == Some("stop")
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        crate::ext_diag!("[ext] cache_warming_decision dispatch failed: {error}");
                    }
                }
            }
        }
        let context = self.warm_context_snapshot(run_system_prompt).await;
        if context.messages.is_empty() {
            return;
        }
        let registry = Arc::clone(&self.registry);
        // The warm request replays the CURRENT model, so it must carry that
        // model's provider credential (not a key captured for another
        // provider at construction — the same stale-key bug the request path
        // fixed).
        let api_key = self.request_api_key_for(&model);
        let idle_delay = match warmer.mode {
            CacheWarmingMode::Idle => Some(warmer.idle_delay),
            _ => None,
        };
        self.spawn_cache_warm_task(registry, model, context, api_key, idle_delay);
    }

    /// Spawn the bounded warm task (both modes): detached from the settled
    /// run, so warming never delays prompt() completion, and abortable — the
    /// live warm's signal is registered on the session and aborted by the
    /// next prompt the moment the conversation goes stale (upstream aborts
    /// in-flight refreshes on new activity).
    fn spawn_cache_warm_task(
        &self,
        registry: Arc<crate::ai::provider::ProviderRegistry>,
        model: crate::ai::types::Model,
        context: crate::ai::types::Context,
        api_key: Option<String>,
        idle_delay: Option<std::time::Duration>,
    ) {
        let warm_signal = AbortSignal::new();
        // Register the live signal so prompt() can abort it directly; the
        // entry is replaced (never cleared) by the next warm.
        *self
            .warm_abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(warm_signal.clone());
        // Warm requests ride the session's transport hooks exactly like
        // conversational streams (upstream re-sends the recorded options
        // through the same streamFn — cache-warmer.ts:332-339).
        let hooks = self.stream_options(None).hooks;
        let session = self.session.clone();
        let plugins = self.plugins.clone();
        let session_events = Arc::clone(&self.session_events);
        tokio::spawn(async move {
            if let Some(delay) = idle_delay {
                tokio::time::sleep(delay).await;
            }
            // prompt() aborts the registered signal on staleness;
            // `warm_once_with_hooks` races the request against it and returns
            // `None` for a stale warm.
            if let Some(message) = crate::cache_warmer::warm_once_with_hooks(
                &registry,
                &model,
                &context,
                api_key.as_deref(),
                Some(warm_signal),
                hooks,
            )
            .await
            {
                if let Some(session) = &session {
                    // Upstream `appendUsage("cache_warm", …)` then
                    // `onWarmed(entry)` → `entry_appended`
                    // (agent-session.ts:495-497): persist first and emit the
                    // stored entry, still off the run's critical path.
                    let appended = {
                        let mut session = session.lock().await;
                        match session.append_usage(
                            crate::cache_warmer::USAGE_KIND_CACHE_WARM,
                            &message.provider,
                            &message.model,
                            message.usage,
                            None,
                        ) {
                            Ok(entry_id) => session
                                .entry(&entry_id)
                                .and_then(|entry| serde_json::to_value(entry).ok()),
                            Err(error) => {
                                crate::ext_diag!(
                                    "[session] failed to record cache-warm usage: {error}"
                                );
                                None
                            }
                        }
                    };
                    if let Some(entry) = appended {
                        crate::agent_session::notifications::emit_entry_appended_wire(
                            plugins.as_deref(),
                            &session_events,
                            entry,
                        )
                        .await;
                    }
                }
            }
        });
    }

    /// The replayed conversation for the warm request: the current projection
    /// (which already includes the just-persisted assistant turn) plus THIS
    /// run's system prompt — the exact prefix the provider is asked to cache.
    /// The run's prompt is passed in because `before_agent_start` may have
    /// overridden it for the settled run: warming with the base
    /// `self.system_prompt` would replay a different prefix and guarantee a
    /// cache miss. Public for tests/.
    pub async fn warm_context_snapshot(
        &self,
        run_system_prompt: Option<String>,
    ) -> crate::ai::types::Context {
        let messages = match &self.session {
            Some(session) => session
                .lock()
                .await
                .build_session_projection()
                .map(|projection| projection.messages)
                .unwrap_or_default(),
            None => history_owned(self.history_snapshot()),
        };
        crate::ai::types::Context {
            system_prompt: run_system_prompt,
            messages,
            tools: None,
        }
    }
    /// Install the cache-warming configuration (upstream `cacheWarming`,
    /// default `"streaming"`). `None` disables warming — the default for
    /// direct SDK sessions, so warming is strictly opt-in per host. Hosts
    /// resolve the mode from settings via
    /// [`crate::cache_warmer::mode_from_settings`].
    pub fn set_cache_warming(&self, settings: Option<crate::cache_warmer::CacheWarmingSettings>) {
        *self
            .cache_warming
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    }
}

/// Upstream `CacheWarmer.evaluate` economics (cache-warmer.ts:378-399) for
/// the `cache_warming_decision` payload.
///
/// - `promptTokens`: the most recent real request's prompt size, as reported
///   by the provider (`input + cacheRead + cacheWrite`).
/// - `warmCost`: a cache read of that prompt plus one output token.
/// - `missCost`: the extra price of the next real request if the entry is
///   lost (cache write when the model prices it, otherwise full input, minus
///   the cache-hit price).
/// - `continuationProbability`: upstream's `IDLE_CONTINUATION_PROBABILITY`
///   (0.15) for the idle mode, 1 while the request is still the active run.
fn cache_warming_economics(
    model: &Model,
    terminal: &AssistantMessage,
    mode: crate::config::settings::CacheWarmingMode,
) -> (f64, f64, f64) {
    let prompt_tokens = terminal
        .usage
        .input
        .saturating_add(terminal.usage.cache_read)
        .saturating_add(terminal.usage.cache_write);
    let mut hit = crate::ai::types::Usage {
        cache_read: prompt_tokens,
        ..crate::ai::types::Usage::default()
    };
    let cache_hit_cost = crate::ai::types::calculate_cost(model, &mut hit).total;
    let mut miss = if model.cost.rates.cache_write > 0.0 {
        crate::ai::types::Usage {
            cache_write: prompt_tokens,
            ..crate::ai::types::Usage::default()
        }
    } else {
        crate::ai::types::Usage {
            input: prompt_tokens,
            ..crate::ai::types::Usage::default()
        }
    };
    let cache_miss_cost = crate::ai::types::calculate_cost(model, &mut miss).total;
    let mut warm = crate::ai::types::Usage {
        cache_read: prompt_tokens,
        output: 1,
        ..crate::ai::types::Usage::default()
    };
    let warm_cost = crate::ai::types::calculate_cost(model, &mut warm).total;
    let miss_cost = (cache_miss_cost - cache_hit_cost).max(0.0);
    let continuation_probability = if mode == crate::config::settings::CacheWarmingMode::Idle {
        0.15
    } else {
        1.0
    };
    (warm_cost, miss_cost, continuation_probability)
}
