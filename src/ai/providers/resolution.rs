//! Model resolution, scoping, and initial selection (upstream
//! `core/model-resolver.ts`).
//!
//! [`find_initial_model`] implements the upstream priority order:
//! 1. CLI args (`--model`, optionally restricted by `--provider`)
//! 2. First model from the scoped list (`--models` / `settings.enabledModels`,
//!    skipped when continuing/resuming)
//! 3. Saved defaults from settings (`defaultProvider`/`defaultModel`, must
//!    have auth)
//! 4. Known-provider default id, then the first available model with auth
//!    (the "restored from session" step belongs to the session host, not
//!    here).
//!
//! Every reference (exact `provider/id`, bare id, scoped pattern) resolves
//! through [`resolve_model_reference`], the single selection policy ported
//! from upstream. Pir-specific difference: upstream resolves `--model` in
//! `main.ts` before `findInitialModel`, while pir's CLI callers (`print.rs`,
//! `sdk/builder.rs`) still expect [`find_initial_model`] to apply CLI args
//! first, so that step stays here and also goes through the shared resolver.

use std::collections::BTreeMap;

use crate::ai::types::{Model, ThinkingLevel};

/// The default chat thinking level (upstream `DEFAULT_THINKING_LEVEL` from
/// `core/defaults.ts`).
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// Default chat model ids per provider (upstream `defaultModelPerProvider`,
/// model-resolver.ts:20-59).
pub fn default_model_per_provider() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
        ("ant-ling", "Ring-2.6-1T"),
        ("anthropic", "claude-opus-4-8"),
        ("openai", "gpt-5.5"),
        ("azure", "gpt-5.4"),
        ("openai-codex", "gpt-6.1-sol"),
        ("radius", "balanced"),
        ("nvidia", "nvidia/nemotron-3.5-super-vl-120b-a12b"),
        ("deepseek", "deepseek-v4-pro"),
        ("google", "gemini-3.1-pro-preview"),
        ("google-vertex", "gemini-3.1-pro-preview"),
        ("github-copilot", "gpt-5.4"),
        ("openrouter", "moonshotai/kimi-k2.6"),
        ("vercel-ai-gateway", "zai/glm-5.1"),
        ("xai", "grok-4.7"),
        ("groq", "openai/gpt-oss-120b"),
        ("cerebras", "gpt-oss-120b"),
        ("zai", "glm-5.3"),
        ("zai-coding-cn", "glm-5.3"),
        ("mistral", "devstral-medium-latest"),
        ("minimax", "MiniMax-M2.7"),
        ("minimax-cn", "MiniMax-M2.7"),
        ("moonshotai", "kimi-k2.6"),
        ("moonshotai-cn", "kimi-k2.6"),
        ("huggingface", "moonshotai/Kimi-K2.6"),
        ("fireworks", "accounts/fireworks/models/kimi-k3"),
        ("together", "moonshotai/Kimi-K3"),
        ("baseten", "zai-org/GLM-5.2"),
        ("opencode", "kimi-k2.6"),
        ("opencode-go", "kimi-k3"),
        ("kimi-coding", "kimi-for-coding"),
        ("meta", "muse-spark-1.3"),
        ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
        (
            "cloudflare-ai-gateway",
            "workers-ai/@cf/moonshotai/kimi-k2.6",
        ),
        ("qwen-token-plan", "qwen3.7-max"),
        ("qwen-token-plan-cn", "qwen3.7-max"),
        ("qwen-token-plan-individual", "qwen3.8-max"),
        ("xiaomi", "mimo-v2.6-pro"),
        ("xiaomi-token-plan-cn", "mimo-v2.6-pro"),
        ("xiaomi-token-plan-ams", "mimo-v2.6-pro"),
        ("xiaomi-token-plan-sgp", "mimo-v2.6-pro"),
    ])
}

/// Whether a string is a valid thinking level (upstream
/// `isValidThinkingLevel`).
fn is_valid_thinking_level(level: &str) -> bool {
    matches!(
        level,
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

/// Parse a thinking-level string (upstream wire names).
pub fn parse_thinking_level(level: &str) -> Option<ThinkingLevel> {
    Some(match level {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        "max" => ThinkingLevel::Max,
        _ => return None,
    })
}

/// A pattern-scoped model with its optional explicit thinking level (upstream
/// `ScopedModel`).
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    /// Thinking level if explicitly specified in the pattern (`"model:high"`).
    pub thinking_level: Option<ThinkingLevel>,
}

/// Check whether a model id looks like an alias (no date suffix, no
/// `-latest` count against it), per upstream `isAlias`.
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    // Date pattern `-YYYYMMDD` marks a dated version.
    let bytes = id.as_bytes();
    if bytes.len() >= 9 {
        let (prefix, suffix) = bytes.split_at(bytes.len() - 8);
        return !(prefix.last() == Some(&b'-') && suffix.iter().all(u8::is_ascii_digit));
    }
    true
}

/// How a trailing `:<suffix>` is treated when resolving a model reference
/// (upstream `ModelReferenceRequest.thinkingSuffix`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSuffixMode {
    /// The suffix stays part of the model id.
    None,
    /// A valid thinking level is parsed; any other suffix stays part of the id.
    Strict,
    /// Like [`ThinkingSuffixMode::Strict`], but an invalid suffix is dropped
    /// with a warning.
    Lenient,
}

/// A model reference to resolve (upstream `ModelReferenceRequest`).
#[derive(Debug, Clone, Copy)]
pub struct ModelReferenceRequest<'a> {
    /// Bare model id or `provider/id`, optionally followed by `:<thinking>`.
    pub text: &'a str,
    /// Only consider models from this provider.
    pub provider: Option<&'a str>,
    /// How to treat a trailing `:<suffix>` (see [`ThinkingSuffixMode`]).
    pub thinking_suffix: ThinkingSuffixMode,
    /// Match id and name substrings when no exact match exists.
    pub fuzzy: bool,
}

/// The provider/model a not-found reference pointed at, used to build a custom
/// model id (upstream `ModelReferenceTarget`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelReferenceTarget {
    pub provider: String,
    pub provider_models: Vec<Model>,
    /// Text after the provider prefix as typed, including any `:<suffix>`.
    pub model_text: String,
    /// `model_text` without a valid thinking suffix (unless the mode is
    /// [`ThinkingSuffixMode::None`]).
    pub model_id: String,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Outcome of resolving one model reference (upstream
/// `ModelReferenceResult`).
#[derive(Debug, Clone, PartialEq)]
pub enum ModelReferenceResult {
    Resolved {
        /// Boxed: `Model` is much larger than the other variants' payloads.
        model: Box<Model>,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    Ambiguous {
        candidates: Vec<Model>,
        usable_count: usize,
        thinking_level: Option<ThinkingLevel>,
        warning: Option<String>,
    },
    NotFound {
        /// Set when a provider is explicit or named by a `provider/` prefix,
        /// for building a custom model id.
        target: Option<ModelReferenceTarget>,
    },
}

fn resolved(model: &Model) -> ModelReferenceResult {
    ModelReferenceResult::Resolved {
        model: Box::new(model.clone()),
        thinking_level: None,
        warning: None,
    }
}

fn not_found() -> ModelReferenceResult {
    ModelReferenceResult::NotFound { target: None }
}

/// Split a valid `:<thinking>` suffix from `text`, preferring the full text
/// first (ids such as OpenRouter's `qwen/qwen3-coder:exacto` contain colons).
fn split_thinking_suffix(text: &str) -> Option<(String, ThinkingLevel)> {
    let (base, suffix) = text.rsplit_once(':')?;
    if !is_valid_thinking_level(suffix) {
        return None;
    }
    Some((base.to_string(), parse_thinking_level(suffix)?))
}

/// Split `provider/rest` when `provider` names a known provider.
fn split_provider_prefix(text: &str, models: &[Model]) -> Option<(String, String)> {
    let (prefix, model_text) = text.split_once('/')?;
    let prefix = prefix.trim().to_lowercase();
    let model_text = model_text.trim();
    if prefix.is_empty() || model_text.is_empty() {
        return None;
    }
    let provider = models
        .iter()
        .find(|model| model.provider.to_lowercase() == prefix)?
        .provider
        .clone();
    Some((provider, model_text.to_string()))
}

fn all_same_provider(models: &[&Model]) -> bool {
    match models.first() {
        Some(first) => models.iter().all(|model| model.provider == first.provider),
        None => true,
    }
}

/// Prefer aliases (`claude-sonnet-4-5`) over dated versions
/// (`claude-sonnet-4-5-20250929`), then the latest (upstream
/// `bestFuzzyMatch`).
fn best_fuzzy_match<'a>(matches: &[&'a Model]) -> Option<&'a Model> {
    let aliases: Vec<&Model> = matches
        .iter()
        .copied()
        .filter(|model| is_alias(&model.id))
        .collect();
    let pool: &[&Model] = if aliases.is_empty() {
        matches
    } else {
        &aliases
    };
    pool.iter().copied().max_by(|a, b| a.id.cmp(&b.id))
}

fn fuzzy_matches<'a>(text: &str, models: &'a [Model]) -> Vec<&'a Model> {
    let lower = text.to_lowercase();
    models
        .iter()
        .filter(|model| {
            model.id.to_lowercase().contains(&lower) || model.name.to_lowercase().contains(&lower)
        })
        .collect()
}

/// `usable` only breaks ties between providers, so a unique match resolves even
/// without credentials (`--api-key` first-time setup).
pub fn resolve_model_reference(
    request: ModelReferenceRequest<'_>,
    models: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    let text = request.text.trim();
    if text.is_empty() {
        return not_found();
    }
    let constraint = request.provider.map(str::to_lowercase);
    let pool: Vec<Model> = match &constraint {
        Some(provider) => models
            .iter()
            .filter(|model| model.provider.to_lowercase() == *provider)
            .cloned()
            .collect(),
        None => models.to_vec(),
    };
    let constrained = ModelReferenceRequest { text, ..request };
    let result = resolve_text(text, constrained, &pool, usable);
    if !matches!(result, ModelReferenceResult::NotFound { .. }) {
        return result;
    }

    let split = split_provider_prefix(text, &pool);
    let anchor: Option<&Model> = if constraint.is_some() {
        pool.first()
    } else {
        match &split {
            Some((provider, _)) => pool.iter().find(|model| &model.provider == provider),
            None => None,
        }
    };
    let Some(anchor) = anchor else {
        return result;
    };
    let model_text = match &split {
        Some((_, text)) => text.clone(),
        None => text.to_string(),
    };
    let suffix = if request.thinking_suffix == ThinkingSuffixMode::None {
        None
    } else {
        split_thinking_suffix(&model_text)
    };
    let provider = anchor.provider.clone();
    let mut provider_models = vec![(*anchor).clone()];
    provider_models.extend(
        pool.iter()
            .filter(|model| model.provider == provider && !std::ptr::eq(*model, anchor))
            .cloned(),
    );
    let (model_id, thinking_level) = match suffix {
        Some((base, level)) => (base, Some(level)),
        None => (model_text.clone(), None),
    };
    ModelReferenceResult::NotFound {
        target: Some(ModelReferenceTarget {
            provider,
            provider_models,
            model_text,
            model_id,
            thinking_level,
        }),
    }
}

/// `provider/rest` is only a hint: the whole text may also be a model id on
/// another provider, such as gateway id `zai/glm-5`. The named provider wins
/// when usable; otherwise a sole usable raw match wins. An unusable named
/// match is still preferred over unusable raw matches, so `--api-key` can set
/// up the named provider (upstream `selectReference`).
fn select_reference(
    text: &str,
    pool: &[Model],
    fuzzy: bool,
    usable: &dyn Fn(&str) -> bool,
) -> Option<ModelReferenceResult> {
    let lower_text = text.to_lowercase();
    let raw: Vec<&Model> = pool
        .iter()
        .filter(|model| model.id.to_lowercase() == lower_text)
        .collect();
    let split = split_provider_prefix(text, pool);
    let mut named: Option<&Model> = None;
    if let Some((provider, model_text)) = &split {
        let provider_models: Vec<&Model> = pool
            .iter()
            .filter(|model| &model.provider == provider)
            .collect();
        let lower_model_text = model_text.to_lowercase();
        // The provider's own literal id (OpenRouter's `openrouter/free`) beats
        // its fuzzy matches.
        named = provider_models
            .iter()
            .copied()
            .find(|model| model.id.to_lowercase() == lower_model_text)
            .or_else(|| {
                raw.iter()
                    .copied()
                    .find(|model| &model.provider == provider)
            })
            .or_else(|| {
                if !fuzzy {
                    return None;
                }
                let lower = model_text.to_lowercase();
                let matches: Vec<&Model> = provider_models
                    .iter()
                    .copied()
                    .filter(|model| {
                        model.id.to_lowercase().contains(&lower)
                            || model.name.to_lowercase().contains(&lower)
                    })
                    .collect();
                best_fuzzy_match(&matches)
            });
    }

    let others: Vec<&Model> = raw
        .iter()
        .copied()
        .filter(|model| !named.is_some_and(|named| std::ptr::eq(*model, named)))
        .collect();
    let mut candidates: Vec<&Model> = Vec::new();
    if let Some(named) = named {
        candidates.push(named);
    }
    candidates.extend(others.iter().copied());
    if candidates.is_empty() {
        return None;
    }
    if all_same_provider(&candidates) {
        return Some(resolved(candidates[0]));
    }
    if let Some(named) = named {
        if usable(&named.provider) {
            return Some(resolved(named));
        }
    }
    let usable_others: Vec<&Model> = others
        .iter()
        .copied()
        .filter(|model| usable(&model.provider))
        .collect();
    if usable_others.len() == 1 {
        return Some(resolved(usable_others[0]));
    }
    if usable_others.is_empty() {
        if let Some(named) = named {
            return Some(resolved(named));
        }
    }
    Some(ModelReferenceResult::Ambiguous {
        candidates: candidates.into_iter().cloned().collect(),
        usable_count: usable_others.len(),
        thinking_level: None,
        warning: None,
    })
}

fn select_fuzzy(
    matches: Vec<&Model>,
    usable: &dyn Fn(&str) -> bool,
) -> Option<ModelReferenceResult> {
    if matches.is_empty() {
        return None;
    }
    let multiple_providers = !all_same_provider(&matches);
    let usable_matches: Vec<&Model> = if multiple_providers {
        matches
            .iter()
            .copied()
            .filter(|model| usable(&model.provider))
            .collect()
    } else {
        Vec::new()
    };
    let pool: &[&Model] = if usable_matches.is_empty() {
        &matches
    } else {
        &usable_matches
    };
    let best = best_fuzzy_match(pool)?;
    // The same id on several providers would otherwise be decided by catalog
    // order.
    let tied: Vec<&Model> = pool
        .iter()
        .copied()
        .filter(|model| model.id == best.id)
        .collect();
    if all_same_provider(&tied) {
        return Some(resolved(best));
    }
    let usable_count = if usable_matches.is_empty() {
        0
    } else {
        tied.len()
    };
    Some(ModelReferenceResult::Ambiguous {
        candidates: tied.into_iter().cloned().collect(),
        usable_count,
        thinking_level: None,
        warning: None,
    })
}

fn resolve_text(
    text: &str,
    request: ModelReferenceRequest<'_>,
    pool: &[Model],
    usable: &dyn Fn(&str) -> bool,
) -> ModelReferenceResult {
    if let Some(reference) = select_reference(text, pool, request.fuzzy, usable) {
        return reference;
    }
    if request.fuzzy {
        if let Some(fuzzy) = select_fuzzy(fuzzy_matches(text, pool), usable) {
            return fuzzy;
        }
    }
    if request.thinking_suffix == ThinkingSuffixMode::None {
        return not_found();
    }
    if let Some((base, level)) = split_thinking_suffix(text) {
        let inner = resolve_text(&base, request, pool, usable);
        return with_thinking_level(inner, level);
    }
    // Keep an invalid suffix as part of the id and fail, rather than resolve a
    // different model.
    if request.thinking_suffix == ThinkingSuffixMode::Strict {
        return not_found();
    }
    let Some((prefix, suffix)) = text.rsplit_once(':') else {
        return not_found();
    };
    let inner = resolve_text(prefix, request, pool, usable);
    apply_invalid_suffix_warning(inner, suffix, text)
}

fn with_thinking_level(result: ModelReferenceResult, level: ThinkingLevel) -> ModelReferenceResult {
    match result {
        ModelReferenceResult::Resolved {
            model,
            warning,
            thinking_level: _,
        } => ModelReferenceResult::Resolved {
            model,
            // Only use this thinking level if the inner resolution had no
            // warning.
            thinking_level: if warning.is_some() { None } else { Some(level) },
            warning,
        },
        ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            warning,
            thinking_level: _,
        } => ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            thinking_level: if warning.is_some() { None } else { Some(level) },
            warning,
        },
        not_found => not_found,
    }
}

fn apply_invalid_suffix_warning(
    result: ModelReferenceResult,
    suffix: &str,
    text: &str,
) -> ModelReferenceResult {
    let warning = Some(format!(
        "Invalid thinking level \"{suffix}\" in pattern \"{text}\". Using default instead."
    ));
    match result {
        ModelReferenceResult::Resolved { model, .. } => ModelReferenceResult::Resolved {
            model,
            thinking_level: None,
            warning,
        },
        ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            ..
        } => ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            thinking_level: None,
            warning,
        },
        not_found => not_found,
    }
}

/// Exact `provider/id` or bare id lookup without thinking suffixes or fuzzy
/// matching (upstream `findExactModelReferenceMatch`).
pub fn find_exact_model_reference_match(reference: &str, models: &[Model]) -> Option<Model> {
    match resolve_model_reference(
        ModelReferenceRequest {
            text: reference,
            provider: None,
            thinking_suffix: ThinkingSuffixMode::None,
            fuzzy: false,
        },
        models,
        &|_| true,
    ) {
        ModelReferenceResult::Resolved { model, .. } => Some(*model),
        _ => None,
    }
}

/// A glob pattern compiled once for repeated matching with `globset`.
///
/// Semantics (upstream uses `minimatch` with `{ nocase: true }` in
/// `model-resolver.ts`, and so does the scope loop that calls this):
///
/// - `*` matches any run of characters, `/` included: `literal_separator` is
///   off, which preserves the previous hand-rolled matcher's behaviour and
///   lets one pattern cover a `provider/model` pair. Upstream's `minimatch`
///   stops `*` at `/` but probes the pattern against both the pair and the
///   bare id, so [`resolve_model_scope`] doing both here is equivalent in
///   practice.
/// - `?` matches exactly one character.
/// - `**` is recursive, as in `minimatch`: `a/**/b` matches `a/b` and
///   `a/x/y/b`, and `**/b` matches `b`. The old matcher collapsed `**` into
///   `*`, so `a/**/b` needed at least one segment between the slashes; this is
///   the one intentional widening.
/// - `[abc]`, `[a-z]`, `[!abc]` classes and `{a,b}` alternates are supported,
///   as in `minimatch`. The old matcher treated every bracket literally;
///   [`resolve_model_scope`] still exact-resolves a bracketed id such as
///   `bracketed-model[1m]` before globbing, so catalog ids are unaffected.
///   An *unclosed* `[` stays literal (`allow_unclosed_class`), as it did
///   before.
/// - `\` escapes the next character. It is forced on so patterns mean the
///   same thing on Windows (where globset would otherwise treat `\` as a path
///   separator) and Unix. The old matcher had no escape handling.
/// - Matching is case-insensitive. globset folds ASCII only; the old matcher
///   ran `str::to_lowercase` first. Model ids and tool names are ASCII, so
///   the two agree in practice (non-ASCII letters now compare exactly).
///
/// A pattern that fails to parse (an unmatched `{` or `}`, a dangling `\`,
/// an invalid range such as `[z-a]`) falls back to a literal,
/// ASCII-case-insensitive comparison rather than panicking or matching
/// nothing.
#[derive(Debug, Clone)]
pub struct CompiledGlob {
    matcher: Option<globset::GlobMatcher>,
    literal: String,
}

impl CompiledGlob {
    /// Compile `pattern` once; reuse the result with [`Self::is_match`].
    pub fn new(pattern: &str) -> Self {
        let matcher = globset::GlobBuilder::new(pattern)
            .case_insensitive(true)
            // `*`/`?` keep crossing `/` (the old matcher's `*` matched any
            // byte), which also keeps `*sonnet*` matching a full
            // `provider/id` string.
            .literal_separator(false)
            // Pin escape handling: globset's default depends on whether the
            // platform treats `\` as a path separator.
            .backslash_escape(true)
            // Ids such as `bracketed-model[1m]` are exact-resolved before
            // this path; if one falls through, keep the bracket literal.
            .allow_unclosed_class(true)
            .build()
            .ok()
            .map(|glob| glob.compile_matcher());
        Self {
            matcher,
            literal: pattern.to_string(),
        }
    }

    /// Whether `text` matches the pattern (case-insensitive).
    pub fn is_match(&self, text: &str) -> bool {
        match &self.matcher {
            // `Candidate::from_bytes` avoids `Path` conversions; the only
            // normalization it applies is `\` -> `/` on Windows, and model
            // ids/tool names never contain a backslash.
            Some(matcher) => matcher.is_match_candidate(&globset::Candidate::from_bytes(text)),
            None => self.literal.eq_ignore_ascii_case(text),
        }
    }
}

/// Glob match for a single call (compiles the pattern each time; use
/// [`CompiledGlob`] to match one pattern against many strings). See
/// [`CompiledGlob`] for the supported syntax and the documented differences
/// from the removed hand-rolled matcher.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    CompiledGlob::new(pattern).is_match(text)
}

/// Try to match a pattern to a model through the shared selection policy
/// (exact reference first, then a fuzzy id/name match).
pub fn try_match_model(pattern: &str, models: &[Model]) -> Option<Model> {
    match resolve_model_reference(
        ModelReferenceRequest {
            text: pattern,
            provider: None,
            thinking_suffix: ThinkingSuffixMode::None,
            fuzzy: true,
        },
        models,
        &|_| true,
    ) {
        ModelReferenceResult::Resolved { model, .. } => Some(*model),
        _ => None,
    }
}

/// One parsed pattern result (upstream's removed `ParsedModelResult`, kept for
/// the auth provider lookup).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedModelResult {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub warning: Option<String>,
}

/// Parse a pattern through the shared selection policy. Ambiguous references
/// yield no model instead of letting catalog order decide.
pub fn parse_model_pattern(
    pattern: &str,
    models: &[Model],
    allow_invalid_thinking_level_fallback: bool,
) -> ParsedModelResult {
    let thinking_suffix = if allow_invalid_thinking_level_fallback {
        ThinkingSuffixMode::Lenient
    } else {
        ThinkingSuffixMode::Strict
    };
    match resolve_model_reference(
        ModelReferenceRequest {
            text: pattern,
            provider: None,
            thinking_suffix,
            fuzzy: true,
        },
        models,
        &|_| true,
    ) {
        ModelReferenceResult::Resolved {
            model,
            thinking_level,
            warning,
        } => ParsedModelResult {
            model: Some(*model),
            thinking_level,
            warning,
        },
        _ => ParsedModelResult {
            model: None,
            thinking_level: None,
            warning: None,
        },
    }
}

/// Build a fallback model for an unknown id from a provider's known models
/// (upstream `buildFallbackModel`).
fn build_fallback_model(provider_models: &[Model], model_id: &str) -> Option<Model> {
    let first = provider_models.first()?;
    let default_id = default_model_per_provider()
        .get(first.provider.as_str())
        .copied();
    let base = default_id
        .and_then(|default| provider_models.iter().find(|model| model.id == default))
        .unwrap_or(first);
    let mut model = base.clone();
    model.id = model_id.to_string();
    model.name = model_id.to_string();
    Some(model)
}

/// The CLI model resolution outcome: model, parsed thinking level, optional
/// fallback warning (upstream `ResolveCliModelResult`).
type ResolvedCliModel = (Option<Model>, Option<ThinkingLevel>, Option<String>);

fn no_auth(_: &str) -> bool {
    false
}

/// Resolve a single model from CLI flags (upstream `resolveCliModel`):
/// `--provider <p> --model <pattern>`, `--model <provider>/<pattern>`, or a
/// fuzzy match. Uses *all* models (not just ones with auth) so `--api-key`
/// works for first-time setup.
pub fn resolve_cli_model(
    cli_provider: Option<&str>,
    cli_model: &str,
    models: &[Model],
) -> Result<ResolvedCliModel, String> {
    resolve_cli_model_with_auth(cli_provider, cli_model, models, &no_auth, false, None)
}

/// [`resolve_cli_model`] with credential tie-breaking (upstream
/// `resolveCliModel` `modelRuntime.hasConfiguredAuth`). `inline_api_key`
/// disables that tie-break because `--api-key` does not choose a provider.
/// An explicit `cli_thinking` keeps a custom model id's `:<suffix>` intact,
/// since the suffix is then not a thinking level.
pub fn resolve_cli_model_with_auth(
    cli_provider: Option<&str>,
    cli_model: &str,
    models: &[Model],
    has_auth: &dyn Fn(&str) -> bool,
    inline_api_key: bool,
    cli_thinking: Option<ThinkingLevel>,
) -> Result<ResolvedCliModel, String> {
    let usable: &dyn Fn(&str) -> bool = if inline_api_key { &no_auth } else { has_auth };
    let result = resolve_model_reference(
        ModelReferenceRequest {
            text: cli_model,
            provider: cli_provider,
            thinking_suffix: ThinkingSuffixMode::Strict,
            fuzzy: true,
        },
        models,
        usable,
    );
    match result {
        ModelReferenceResult::Resolved {
            model,
            thinking_level,
            warning,
        } => Ok((Some(*model), thinking_level, warning)),
        ModelReferenceResult::Ambiguous {
            candidates,
            usable_count,
            ..
        } => {
            let mut matches: Vec<String> = candidates
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect();
            matches.sort();
            let auth_hint = if inline_api_key {
                "--api-key does not choose a provider."
            } else if usable_count == 0 {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            Err(format!(
                "Model \"{cli_model}\" is ambiguous across providers: {}. {auth_hint} Use --provider or provider/model.",
                matches.join(", ")
            ))
        }
        ModelReferenceResult::NotFound { target: None } => {
            let display = match cli_provider {
                Some(provider) => format!("{provider}/{cli_model}"),
                None => cli_model.to_string(),
            };
            Err(format!(
                "Model \"{display}\" not found. Use --list-models to see available models."
            ))
        }
        ModelReferenceResult::NotFound {
            target: Some(target),
        } => {
            // Use the thinking suffix only when --thinking is not explicitly
            // provided; otherwise it stays part of the custom model id.
            let explicit = cli_thinking.is_some();
            let model_id = if explicit {
                target.model_text.as_str()
            } else {
                target.model_id.as_str()
            };
            let fallback_thinking = if explicit {
                None
            } else {
                target.thinking_level
            };
            let mut fallback =
                build_fallback_model(&target.provider_models, model_id).ok_or_else(|| {
                    format!(
                        "Model \"{cli_model}\" not found. Use --list-models to see available models."
                    )
                })?;
            if cli_thinking
                .or(fallback_thinking)
                .is_some_and(|level| level != ThinkingLevel::Off)
            {
                fallback.reasoning = true;
            }
            let warning = format!(
                "Model \"{model_id}\" not found for provider \"{}\". Using custom model id.",
                target.provider
            );
            Ok((Some(fallback), fallback_thinking, Some(warning)))
        }
    }
}

fn ambiguous_diagnostic(pattern: &str, candidates: &[Model]) -> String {
    let mut providers: Vec<&str> = candidates
        .iter()
        .map(|model| model.provider.as_str())
        .collect();
    providers.sort_unstable();
    providers.dedup();
    format!(
        "Pattern \"{pattern}\" matches several providers: {}. Use provider/id to choose one.",
        providers.join(", ")
    )
}

/// Resolve model patterns to scoped models (upstream
/// `resolveModelScopeFromModels`). Patterns support globs (`anthropic/*`,
/// `*sonnet*`), exact `provider/model` references, fuzzy id/name matches, and
/// an optional `:level` thinking suffix. Every model is usable here, so an id
/// offered by several providers is added for each, with a warning.
pub fn resolve_model_scope(
    patterns: &[String],
    models: &[Model],
) -> (Vec<ScopedModel>, Vec<String>) {
    let mut scoped: Vec<ScopedModel> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();
    let usable = |_: &str| true;
    for pattern in patterns {
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            let (glob_pattern, thinking_level) = match pattern.rsplit_once(':') {
                Some((prefix, suffix)) if is_valid_thinking_level(suffix) => {
                    (prefix, parse_thinking_level(suffix))
                }
                _ => (pattern.as_str(), None),
            };
            // Ids such as `bracketed-model[1m]` are not globs.
            let exact = resolve_model_reference(
                ModelReferenceRequest {
                    text: glob_pattern,
                    provider: None,
                    thinking_suffix: ThinkingSuffixMode::None,
                    fuzzy: false,
                },
                models,
                &usable,
            );
            let matches: Vec<Model> = match exact {
                ModelReferenceResult::Resolved { model, .. } => vec![*model],
                ModelReferenceResult::Ambiguous { candidates, .. } => {
                    diagnostics.push(ambiguous_diagnostic(pattern, &candidates));
                    candidates
                }
                ModelReferenceResult::NotFound { .. } => {
                    // One compiled pattern for the whole catalog scan
                    // (upstream probes `provider/id` and the bare id).
                    let glob = CompiledGlob::new(glob_pattern);
                    models
                        .iter()
                        .filter(|model| {
                            let full = format!("{}/{}", model.provider, model.id);
                            glob.is_match(&full) || glob.is_match(&model.id)
                        })
                        .cloned()
                        .collect()
                }
            };
            if matches.is_empty() {
                diagnostics.push(format!("No models match pattern \"{pattern}\""));
                continue;
            }
            for model in matches {
                push_unique(&mut scoped, model, thinking_level);
            }
            continue;
        }
        let result = resolve_model_reference(
            ModelReferenceRequest {
                text: pattern,
                provider: None,
                thinking_suffix: ThinkingSuffixMode::Lenient,
                fuzzy: true,
            },
            models,
            &usable,
        );
        match result {
            ModelReferenceResult::NotFound { .. } => {
                diagnostics.push(format!("No models match pattern \"{pattern}\""));
            }
            ModelReferenceResult::Resolved {
                model,
                thinking_level,
                warning,
            } => {
                if let Some(warning) = warning {
                    diagnostics.push(warning);
                }
                push_unique(&mut scoped, *model, thinking_level);
            }
            ModelReferenceResult::Ambiguous {
                candidates,
                thinking_level,
                warning,
                ..
            } => {
                if let Some(warning) = warning {
                    diagnostics.push(warning);
                }
                diagnostics.push(ambiguous_diagnostic(pattern, &candidates));
                for model in candidates {
                    push_unique(&mut scoped, model, thinking_level);
                }
            }
        }
    }
    (scoped, diagnostics)
}

fn push_unique(scoped: &mut Vec<ScopedModel>, model: Model, thinking_level: Option<ThinkingLevel>) {
    if !scoped
        .iter()
        .any(|entry| entry.model.provider == model.provider && entry.model.id == model.id)
    {
        scoped.push(ScopedModel {
            model,
            thinking_level,
        });
    }
}

/// A provider's models are "available" when a credential is configured for
/// the provider (the caller's auth check).
type HasConfiguredAuth<'a> = dyn Fn(&str) -> bool + 'a;

/// Inputs to [`find_initial_model`] (upstream `findInitialModel` options).
pub struct InitialModelOptions<'a> {
    /// `--provider` (requires `--model` to take effect).
    pub cli_provider: Option<&'a str>,
    /// `--model <pattern[:thinking]>`.
    pub cli_model: Option<&'a str>,
    /// `--thinking` (overrides the model-pattern suffix).
    pub cli_thinking: Option<ThinkingLevel>,
    /// Patterns from `--models` / `settings.enabledModels`, already resolved.
    pub scoped_models: &'a [ScopedModel],
    /// Whether this run continues/resumes a session (skips the scoped-model
    /// step).
    pub is_continuing: bool,
    /// Settings `defaultProvider`.
    pub default_provider: Option<&'a str>,
    /// Settings `defaultModel`.
    pub default_model_id: Option<&'a str>,
    /// Settings `defaultThinkingLevel` (never `off`).
    pub default_thinking_level: Option<ThinkingLevel>,
    /// Settings `modelThinkingLevels` keyed `provider/modelId`.
    pub model_thinking_levels: &'a BTreeMap<String, ThinkingLevel>,
    /// The full available model snapshot.
    pub models: &'a [Model],
    /// Whether a candidate's provider has configured auth.
    pub has_auth: &'a HasConfiguredAuth<'a>,
    /// `--api-key` was supplied: it goes to whichever provider is selected,
    /// so stored credentials must not break ties between providers.
    pub inline_api_key: bool,
}

fn no_auth_ref(_: &str) -> bool {
    false
}

impl Default for InitialModelOptions<'_> {
    fn default() -> Self {
        static EMPTY_LEVELS: BTreeMap<String, ThinkingLevel> = BTreeMap::new();
        static NO_MODELS: &[Model] = &[];
        static NO_SCOPE: &[ScopedModel] = &[];
        Self {
            cli_provider: None,
            cli_model: None,
            cli_thinking: None,
            scoped_models: NO_SCOPE,
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_thinking_levels: &EMPTY_LEVELS,
            models: NO_MODELS,
            has_auth: &no_auth_ref,
            inline_api_key: false,
        }
    }
}

/// Find the initial model (upstream `findInitialModel`, model-resolver.ts:608-709).
pub fn find_initial_model(options: InitialModelOptions<'_>) -> Result<InitialModelResult, String> {
    let InitialModelOptions {
        cli_provider,
        cli_model,
        cli_thinking,
        scoped_models,
        is_continuing,
        default_provider,
        default_model_id,
        default_thinking_level,
        model_thinking_levels,
        models,
        has_auth,
        inline_api_key,
    } = options;
    // 1. CLI args take priority. `--model` works without `--provider`; the
    // shared resolver then picks the provider by credentials or reports the
    // ambiguity.
    if let Some(model_pattern) = cli_model {
        let (model, thinking, _warning) = resolve_cli_model_with_auth(
            cli_provider,
            model_pattern,
            models,
            has_auth,
            inline_api_key,
            cli_thinking,
        )?;
        // CLI --thinking overrides the pattern suffix.
        let thinking = cli_thinking.or(thinking);
        return Ok(InitialModelResult {
            model,
            thinking_level: thinking.unwrap_or(DEFAULT_THINKING_LEVEL),
            fallback_message: None,
        });
    }

    // 2. First model from the scoped list (skipped when continuing/resuming).
    if !scoped_models.is_empty() && !is_continuing {
        let first = &scoped_models[0];
        let per_model = model_thinking_levels
            .get(&format!("{}/{}", first.model.provider, first.model.id))
            .copied();
        let thinking = first
            .thinking_level
            .or(per_model)
            .or(default_thinking_level)
            .unwrap_or(DEFAULT_THINKING_LEVEL);
        return Ok(InitialModelResult {
            model: Some(first.model.clone()),
            thinking_level: thinking,
            fallback_message: None,
        });
    }

    // 3. Saved defaults from settings, when auth is configured.
    if let (Some(provider), Some(model_id)) = (default_provider, default_model_id) {
        if let Some(model) = models
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
        {
            if has_auth(provider) {
                let per_model = model_thinking_levels
                    .get(&format!("{provider}/{model_id}"))
                    .copied();
                let thinking = per_model
                    .or(default_thinking_level)
                    .unwrap_or(DEFAULT_THINKING_LEVEL);
                return Ok(InitialModelResult {
                    model: Some(model.clone()),
                    thinking_level: thinking,
                    fallback_message: None,
                });
            }
        }
    }

    // 4. Known-provider default id, then the first available model.
    let available: Vec<Model> = models
        .iter()
        .filter(|model| has_auth(&model.provider))
        .cloned()
        .collect();
    let defaults = default_model_per_provider();
    for (provider, default_id) in &defaults {
        if let Some(matched) = available
            .iter()
            .find(|model| &model.provider == provider && &model.id == default_id)
        {
            return Ok(InitialModelResult {
                model: Some(matched.clone()),
                thinking_level: DEFAULT_THINKING_LEVEL,
                fallback_message: None,
            });
        }
    }
    if let Some(first) = available.first() {
        return Ok(InitialModelResult {
            model: Some(first.clone()),
            thinking_level: DEFAULT_THINKING_LEVEL,
            fallback_message: None,
        });
    }

    Ok(InitialModelResult {
        model: None,
        thinking_level: DEFAULT_THINKING_LEVEL,
        fallback_message: None,
    })
}

/// The initial model selection (upstream `InitialModelResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct InitialModelResult {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub fallback_message: Option<String>,
}
