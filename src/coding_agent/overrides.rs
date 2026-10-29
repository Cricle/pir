//! Shared CLI model-selection overrides for the TUI and RPC hosts.
//!
//! Print mode has always honoured the full CLI surface
//! (`--provider/--model/--thinking/--api-key/--models`); the interactive and
//! RPC hosts used to parse those flags and then drop them. This module is the
//! one resolution path both hosts call, so the model chosen by the CLI is the
//! model the session actually drives.

use std::collections::BTreeMap;

use crate::ai::providers::resolution::{
    find_initial_model, resolve_model_scope, InitialModelOptions,
};
use crate::ai::providers::{has_configured_auth, ScopedModel};
use crate::ai::types::{Model, ThinkingLevel};
use crate::config::settings::Settings;

/// The CLI flags that select the starting model and credential.
#[derive(Debug, Clone, Default)]
pub struct CliModelOverrides {
    /// `--provider` (only meaningful together with `--model`).
    pub provider: Option<String>,
    /// `--model <pattern[:thinking]>`.
    pub model: Option<String>,
    /// `--thinking`.
    pub thinking: Option<ThinkingLevel>,
    /// `--api-key` (scoped to the selected provider).
    pub api_key: Option<String>,
    /// `--models` entries (comma-split by the CLI): `None` when the flag was
    /// absent, `Some([])` for an explicit `--models ""`. Upstream replaces the
    /// settings `enabledModels` list with a present CLI list, including the
    /// empty one (`parsed.models ?? settingsManager.getEnabledModels()`).
    pub model_patterns: Option<Vec<String>>,
    /// Whether this run continues/resumes a session (skips the first scoped
    /// model when choosing the starting model, upstream `findInitialModel`).
    pub is_continuing: bool,
}

impl CliModelOverrides {
    /// Whether any model-selection flag was supplied.
    pub fn requested(&self) -> bool {
        self.provider.is_some()
            || self.model.is_some()
            || self.thinking.is_some()
            || self.api_key.is_some()
            || self.model_patterns.is_some()
    }
}

/// The resolved starting model plus the session-scope derived from
/// `--models` / `settings.enabledModels`.
#[derive(Debug, Clone)]
pub struct ResolvedModelSelection {
    pub model: Model,
    /// The explicit `--api-key`, else the models.json key for the selected
    /// provider (the same precedence print uses).
    pub api_key: Option<String>,
    /// The starting thinking level, resolved through the shared policy.
    pub thinking: ThinkingLevel,
    /// Scoped models for Ctrl+P / `setScopedModels`; empty when no scope.
    pub scoped_models: Vec<ScopedModel>,
    /// Diagnostics from `--models` pattern resolution.
    pub diagnostics: Vec<String>,
}

/// Upstream `parsed.models ?? settingsManager.getEnabledModels()` (main.ts:813):
/// a PRESENT CLI `--models` list replaces the settings `enabledModels` scope,
/// including an explicit empty list (`--models ""`), which yields no scoped
/// models instead of falling back to the settings.
///
/// Both hosts (print and the TUI/RPC override path) call this one helper so the
/// replacement rule cannot drift between entry points.
pub fn effective_model_patterns(
    cli_patterns: Option<&[String]>,
    settings_patterns: Option<&[String]>,
) -> Vec<String> {
    cli_patterns
        .map(<[String]>::to_vec)
        .or_else(|| settings_patterns.map(<[String]>::to_vec))
        .unwrap_or_default()
}

/// Resolve the starting model exactly like print mode's `run_print`
/// (upstream `findInitialModel` + `buildSessionOptions`).
pub fn resolve_model(
    overrides: &CliModelOverrides,
    models: &[Model],
    api_keys: &BTreeMap<String, String>,
    settings: &Settings,
) -> crate::Result<ResolvedModelSelection> {
    // Upstream main.ts:831-839: `--api-key` requires an explicit model choice
    // (`--model`, `--provider/--model`, or `--models`).
    if overrides.api_key.is_some()
        && overrides.model.is_none()
        && overrides.provider.is_none()
        && overrides.model_patterns.is_none()
    {
        return Err(anyhow::anyhow!(
            "--api-key requires a model to be specified via --model, --provider/--model, or --models"
        ));
    }
    // Upstream main.ts:813: `parsed.models ?? settingsManager.getEnabledModels()`
    // — a present `--models` REPLACES the settings scope (an explicit
    // `--models ""` yields no scoped models at all).
    let scoped_patterns = effective_model_patterns(
        overrides.model_patterns.as_deref(),
        settings.enabled_models.as_deref(),
    );
    let (scoped_models, diagnostics) = resolve_model_scope(&scoped_patterns, models);

    let empty_model_thinking_levels = BTreeMap::new();
    let has_auth = |provider: &str| has_configured_auth(provider, api_keys);
    let initial = find_initial_model(InitialModelOptions {
        cli_provider: overrides.provider.as_deref(),
        cli_model: overrides.model.as_deref(),
        cli_thinking: overrides.thinking,
        scoped_models: &scoped_models,
        is_continuing: overrides.is_continuing,
        default_provider: settings.default_provider.as_deref(),
        default_model_id: settings.default_model.as_deref(),
        default_thinking_level: settings
            .default_thinking_level
            .filter(|level| *level != ThinkingLevel::Off),
        model_thinking_levels: settings
            .model_thinking_levels
            .as_ref()
            .unwrap_or(&empty_model_thinking_levels),
        models,
        has_auth: &has_auth,
        inline_api_key: overrides.api_key.is_some(),
    })
    .map_err(anyhow::Error::msg)?;

    let Some(model) = initial.model else {
        return Err(anyhow::anyhow!(
            "no model available; configure a provider with `pir auth login` or ~/.pi/agent/models.json"
        ));
    };

    // The starting thinking level, upstream `buildSessionOptions` ->
    // `createAgentSession`: explicit `--thinking`, else the `--model`
    // pattern's suffix, else the first scoped model's suffix, then the
    // per-model override, then `defaultThinkingLevel`, then the shared
    // default. Mirrors `print.rs`.
    let thinking = {
        let explicit = match overrides.model.as_deref() {
            Some(pattern) => {
                let (_, pattern_thinking, _) =
                    crate::ai::providers::resolution::resolve_cli_model_with_auth(
                        overrides.provider.as_deref(),
                        pattern,
                        models,
                        &has_auth,
                        overrides.api_key.is_some(),
                        overrides.thinking,
                    )
                    .map_err(anyhow::Error::msg)?;
                overrides.thinking.or(pattern_thinking)
            }
            None if !scoped_models.is_empty() && !overrides.is_continuing => {
                scoped_models[0].thinking_level
            }
            None => None,
        };
        let per_model = settings.model_thinking_levels.as_ref().and_then(|levels| {
            levels
                .get(&format!("{}/{}", model.provider, model.id))
                .copied()
        });
        crate::agent_session::resolve_starting_thinking_level(
            explicit,
            per_model,
            settings.default_thinking_level,
        )
    };

    let api_key = overrides
        .api_key
        .clone()
        .or_else(|| api_keys.get(&model.provider).cloned());

    Ok(ResolvedModelSelection {
        model,
        api_key,
        thinking,
        scoped_models,
        diagnostics,
    })
}

/// `provider/id` references for the TUI's scoped-model mirror.
pub fn scoped_refs(scoped: &[ScopedModel]) -> Vec<String> {
    scoped
        .iter()
        .map(|entry| format!("{}/{}", entry.model.provider, entry.model.id))
        .collect()
}

/// Resolve `--system-prompt` with upstream `resolvePromptInput` semantics: a
/// value naming an existing path is read as a file, otherwise it is literal.
pub fn resolve_prompt_input(input: &str) -> String {
    let path = std::path::PathBuf::from(input);
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(text) => return text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(),
            Err(error) => {
                eprintln!("Warning: Could not read system prompt file {input}: {error}");
            }
        }
    }
    input.to_owned()
}

/// Resolve one `--append-system-prompt` entry (text or `@file` path).
pub fn resolve_text_or_file(entry: &str) -> crate::Result<String> {
    let path = std::path::PathBuf::from(entry);
    if path.is_file() {
        let text = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!("cannot read --append-system-prompt file {entry}: {error}")
        })?;
        return Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned());
    }
    Ok(entry.to_string())
}
