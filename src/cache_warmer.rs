//! Prompt-cache warming (upstream `packages/coding-agent/src/core/
//! cache-warmer.ts` + the `cacheWarming` setting, default `"streaming"`).
//!
//! After a successful conversational response whose usage reports a prompt
//! cache write, ONE warm-up request replays the projected conversation with
//! the same model and a one-token output cap, so the provider's prompt cache
//! holds the prefix for the next turn. Successful warm requests are recorded
//! as session usage entries with kind [`USAGE_KIND_CACHE_WARM`] (upstream
//! `appendUsage("cache_warm", ...)`), which do not participate in LLM
//! context.
//!
//! Modes (upstream `cacheWarming`):
//! - [`CacheWarmingMode::Off`] — never warm.
//! - [`CacheWarmingMode::Streaming`] — warm immediately after the run
//!   settles (the post-response variant; upstream warms during the response
//!   tail, but pir's loop settles the run before the host regains control —
//!   the post-response seam is the safe timing point and is documented in
//!   [`crate::agent_session`]).
//! - [`CacheWarmingMode::Idle`] — warm after an idle delay; the bounded task
//!   is cancelled by the next prompt (a generation counter).
//!
//! Hard guarantees: never warm on an error/aborted response, never warm a
//! model whose replay would derive different cache-key inputs (upstream
//! `isReplayable` — Anthropic budget-based thinking derives `budget_tokens`
//! from `max_tokens`), at most one warm per turn, warm requests never
//! recurse into warming (they bypass the agent loop entirely and go straight
//! to the provider registry), and an in-flight warm is abortable via the
//! [`AbortSignal`] threaded into [`warm_once`] (upstream aborts in-flight
//! refreshes on new activity).

use std::time::Duration;

use crate::ai::provider::ProviderRegistry;
use crate::ai::types::{
    AbortSignal, AssistantMessage, Context, Model, StopReason, StreamOptions, ThinkingLevel,
};

/// Session usage kind of a warm request (upstream `"cache_warm"`).
pub const USAGE_KIND_CACHE_WARM: &str = "cache_warm";

/// Idle-mode warm delay when the host does not pin one (upstream refreshes
/// at 90% of the model's prompt-cache TTL; pir's models carry no
/// `promptCache` metadata, so a fixed conservative delay is used).
pub const DEFAULT_IDLE_DELAY: Duration = Duration::from_secs(60);

/// Outer bound on one warm request (best-effort; never blocks a session
/// longer than this).
pub const WARM_TIMEOUT: Duration = Duration::from_secs(60);

/// Resolved cache-warming configuration a host installs on the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheWarmingSettings {
    pub mode: crate::config::settings::CacheWarmingMode,
    pub idle_delay: Duration,
}

impl CacheWarmingSettings {
    /// Settings for one mode with the default idle delay.
    pub fn new(mode: crate::config::settings::CacheWarmingMode) -> Self {
        Self {
            mode,
            idle_delay: DEFAULT_IDLE_DELAY,
        }
    }
}

/// The host-side settings consumer (upstream reads `cacheWarming` in the
/// session host): resolve the mode from the merged settings snapshot the
/// hosts already load. `SettingsManager::cache_warming_mode()` applies the
/// upstream default `"streaming"` when the setting is unset.
pub fn mode_from_settings(
    manager: &crate::config::settings::SettingsManager,
) -> crate::config::settings::CacheWarmingMode {
    manager.cache_warming_mode()
}

/// Whether the terminal message of a settled run justifies one warm request.
///
/// pir's [`Model`] carries no upstream `promptCache` metadata field, so the
/// provider-cache gate is the adapter's own report: the response usage must
/// show a cache write (the provider accepted and cached the prefix). Error
/// and aborted responses never warm.
pub fn should_warm(terminal: &AssistantMessage, signal_aborted: bool) -> bool {
    if signal_aborted {
        return false;
    }
    if matches!(
        terminal.stop_reason,
        StopReason::Error | StopReason::Aborted | StopReason::Pending
    ) {
        return false;
    }
    // "Usage shows cacheable input": the provider reported a cache write for
    // this request (a read-only cache hit also implies a cached prefix; the
    // write is the cheapest reliable signal that replaying is worthwhile).
    terminal.usage.cache_write > 0 || terminal.usage.cache_read > 0
}

/// Whether replaying the request with a one-token output cap leaves the
/// model's prompt-cache entry untouched (upstream `isReplayable`).
///
/// Anthropic's budget-based thinking (Claude models without adaptive
/// thinking) derives `budget_tokens` from `max_tokens`: the one-token replay
/// would get a different budget, which Anthropic keys the message cache on,
/// and the model could still think for thousands of tokens. Such models are
/// NOT replayable — warming is skipped. A replay is safe when the original
/// request carried no reasoning option, when the model is not on the
/// `anthropic-messages` API, or when the model forces adaptive thinking
/// (`compat.forceAdaptiveThinking`), where no per-request budget is derived.
pub fn is_replayable(model: &Model, reasoning: Option<ThinkingLevel>) -> bool {
    if reasoning.is_none() || model.api != "anthropic-messages" {
        return true;
    }
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.force_adaptive_thinking)
        == Some(true)
}

/// Fire the warm-up request: replay `context` with `model` and a one-token
/// output cap so the provider caches the prefix. Returns `None` when the
/// warm request failed, was aborted via `signal` (checked before the request,
/// mid-flight, and at completion), or terminated with an error/abort
/// (best-effort — cache warming must never affect the active agent run).
///
/// Max retries are disabled and the thinking level is forced off: a replay
/// that could re-think for thousands of tokens would not be a cheap cache
/// refresh (upstream's `isReplayable` guard, [`is_replayable`], gates which
/// models may be replayed at all). The `signal` rides the request
/// ([`StreamOptions::signal`]) so the transport can cancel it, and the wait
/// races against the signal so a stale warm returns promptly even when the
/// provider ignores cancellation.
pub async fn warm_once(
    registry: &ProviderRegistry,
    model: &Model,
    context: &Context,
    api_key: Option<&str>,
    signal: Option<AbortSignal>,
) -> Option<AssistantMessage> {
    warm_once_with_hooks(
        registry,
        model,
        context,
        api_key,
        signal,
        crate::ai::types::ProviderStreamHooks::default(),
    )
    .await
}

/// [`warm_once`] with explicit per-request transport hooks: upstream's warm
/// replay re-sends the recorded request options through the same streamFn the
/// session uses (cache-warmer.ts:332-339), so the extension
/// `before_provider_request` / `after_provider_response` hooks fire for warm
/// requests exactly like conversational ones (sdk.ts:420-434).
pub async fn warm_once_with_hooks(
    registry: &ProviderRegistry,
    model: &Model,
    context: &Context,
    api_key: Option<&str>,
    signal: Option<AbortSignal>,
    hooks: crate::ai::types::ProviderStreamHooks,
) -> Option<AssistantMessage> {
    if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
        return None;
    }
    let options = StreamOptions {
        max_tokens: Some(1),
        max_retries: Some(0),
        reasoning: Some(ThinkingLevel::Off),
        signal: signal.clone(),
        api_key: api_key.map(str::to_owned),
        hooks,
        ..StreamOptions::default()
    };
    let stream = registry.stream_resolving_auth(model.clone(), context.clone(), options);
    let awaited = async {
        // Completion consumer: drain while waiting (the bounded event queue
        // parks a producer whose events are never popped; see
        // `drain_and_result`).
        match tokio::time::timeout(WARM_TIMEOUT, stream.drain_and_result()).await {
            Ok(Some(message)) => Some((*message).clone()),
            _ => None,
        }
    };
    let message = match &signal {
        Some(signal) => {
            tokio::select! {
                message = awaited => message,
                _ = signal.cancelled() => None,
            }
        }
        None => awaited.await,
    };
    let message = message?;
    // An abort that landed at any point fails the warm even if the provider
    // managed to answer.
    if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
        return None;
    }
    if matches!(
        message.stop_reason,
        StopReason::Error | StopReason::Aborted | StopReason::Pending
    ) {
        return None;
    }
    Some(message)
}

/// Record a successful warm request as a session usage entry (upstream
/// `appendUsage("cache_warm", ...)`). Best-effort: persistence failures are
/// ignored — the entry never participates in LLM context.
pub async fn record_warm_usage(
    session: &std::sync::Arc<tokio::sync::Mutex<crate::session_manager::SessionManager>>,
    message: &AssistantMessage,
) {
    let mut session = session.lock().await;
    let _ = session.append_usage(
        USAGE_KIND_CACHE_WARM,
        &message.provider,
        &message.model,
        message.usage,
        None,
    );
}
