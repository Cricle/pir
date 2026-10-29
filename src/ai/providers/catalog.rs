//! Built-in provider catalog: pir's embedded snapshot of pi's built-in model
//! catalog, one JSON resource per provider.
//!
//! Upstream generates its catalog data at build time
//! (`scripts/generate-models.ts` from models.dev + provider APIs) and does not
//! commit the per-provider JSON at HEAD, so pir embeds a curated snapshot
//! assembled from the upstream provider definitions (`providers/*.ts`: exact
//! ids, display names, base URLs, api ids, env-var auth) and the generator's
//! committed constants (OpenAI standard costs, Kimi implied costs, compat
//! rules). Model entries follow the upstream `Model` wire shape (camelCase).
//!
//! Schema (per file): `{ "name": <display>, "baseUrl": <url>,
//! "models": { "<key>": { id, name, api, type?, reasoning, input, output?,
//! cost, contextWindow, maxTokens, thinkingLevelMap?, compat? } } }`.
//! Chat entries (the default) key by model id; `image`/`classifier` entries
//! key as `"<type>:<id>"` (upstream groups the generated catalog per type)
//! and carry `type` plus the kind-specific fields (`output` for image,
//! `contextWindow` for classifier).

pub mod openrouter;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{
    ClassifierModel, ImageModel, InputModality, Model, ModelCost, ModelCostRates, ModelCostTier,
    ModelKind, OpenAiCompat, OutputModality,
};

/// One embedded provider catalog file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogProvider {
    /// Display name (falls back to the provider id).
    #[serde(default)]
    pub name: Option<String>,
    /// Provider-level base URL (each model's `baseUrl`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Models keyed by id.
    #[serde(default)]
    pub models: BTreeMap<String, CatalogModel>,
}

/// Provider-wide static request headers from the upstream generated catalog
/// (`scripts/generate-models.ts` constants). Merged UNDER per-model headers.
fn static_headers_for(provider: &str) -> Option<BTreeMap<String, String>> {
    match provider {
        "github-copilot" => Some(BTreeMap::from([
            (
                "User-Agent".to_string(),
                "GitHubCopilotChat/0.35.0".to_string(),
            ),
            ("Editor-Version".to_string(), "vscode/1.107.0".to_string()),
            (
                "Editor-Plugin-Version".to_string(),
                "copilot-chat/0.35.0".to_string(),
            ),
            (
                "Copilot-Integration-Id".to_string(),
                "vscode-chat".to_string(),
            ),
        ])),
        "nvidia" => Some(BTreeMap::from([(
            "NVCF-POLL-SECONDS".to_string(),
            "3600".to_string(),
        )])),
        _ => None,
    }
}

/// Merge the provider's static headers under a model's own headers (the model
/// entry wins per name).
fn with_static_headers(
    provider: &str,
    model_headers: Option<BTreeMap<String, String>>,
) -> Option<BTreeMap<String, String>> {
    let static_headers = static_headers_for(provider);
    match (static_headers, model_headers) {
        (None, None) => None,
        (Some(static_headers), None) => Some(static_headers),
        (None, Some(model_headers)) => Some(model_headers),
        (Some(mut static_headers), Some(model_headers)) => {
            for (name, value) in model_headers {
                crate::ai::types::insert_header(&mut static_headers, &name, value);
            }
            Some(static_headers)
        }
    }
}

/// One catalog model entry (the upstream `Model` shape, minus provider; the
/// `type` field selects the kind — chat by default, `image` / `classifier`
/// for the non-chat operations).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogModel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub api: String,
    /// Model kind (upstream `Model.type`; absent = chat).
    #[serde(default, rename = "type")]
    pub kind: Option<ModelKind>,
    /// Per-model base URL override (providers with mixed-adapter endpoints,
    /// e.g. Fireworks' anthropic-compatible `/inference` vs `/inference/v1`).
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Option<Vec<String>>,
    /// Output modalities of an `image` entry (upstream `ImageModel.output`).
    #[serde(default)]
    pub output: Option<Vec<String>>,
    #[serde(default)]
    pub cost: Option<CatalogCost>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    /// Static per-model request headers (upstream generated catalog `headers`;
    /// e.g. Copilot/NVIDIA defaults). Merged under request headers by the
    /// adapters.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<OpenAiCompat>,
}

/// Catalog cost block (`ModelCost` in the models.json camelCase shape).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CatalogCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub tiers: Vec<CatalogCostTier>,
}

/// A catalog cost tier.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogCostTier {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    pub input_tokens_above: u64,
}

impl CatalogModel {
    /// The entry's kind (absent `type` = chat, like upstream).
    fn kind(&self) -> ModelKind {
        self.kind.unwrap_or_default()
    }

    /// Convert into a runtime [`Model`], filling the upstream defaults
    /// (4096 context/maxTokens when the catalog omits them, `["text"]` input).
    fn into_model(self, provider: &str, provider_base_url: &str) -> AiResult<Model> {
        let id = self.id;
        if id.is_empty() {
            return Err(AiError::Authentication(format!(
                "catalog model for provider {provider} is missing an id"
            )));
        }
        let cost = self.cost.unwrap_or_default();
        // The embedded snapshot carries `inputLimits`/`promptCache` inside the
        // compat block (like the upstream generated catalog); mirror them onto
        // the runtime model's top-level fields, where upstream declares them.
        let mut compat = self.compat;
        let input_limits = compat
            .as_mut()
            .and_then(|compat| compat.input_limits.take());
        let prompt_cache = compat
            .as_mut()
            .and_then(|compat| compat.prompt_cache.take());
        Ok(Model {
            id: id.clone(),
            name: self.name.unwrap_or_else(|| id.clone()),
            api: self.api,
            provider: provider.to_string(),
            base_url: self
                .base_url
                .unwrap_or_else(|| provider_base_url.to_string()),
            reasoning: self.reasoning,
            thinking_level_map: self.thinking_level_map,
            input: self.input.unwrap_or_else(|| vec!["text".to_string()]),
            cost: ModelCost {
                rates: ModelCostRates {
                    input: cost.input,
                    output: cost.output,
                    cache_read: cost.cache_read,
                    cache_write: cost.cache_write,
                },
                tiers: cost
                    .tiers
                    .into_iter()
                    .map(|tier| ModelCostTier {
                        rates: ModelCostRates {
                            input: tier.input,
                            output: tier.output,
                            cache_read: tier.cache_read,
                            cache_write: tier.cache_write,
                        },
                        input_tokens_above: tier.input_tokens_above,
                    })
                    .collect(),
            },
            context_window: self.context_window.unwrap_or(4096),
            max_tokens: self.max_tokens.unwrap_or(4096),
            sampling_params: None,
            headers: with_static_headers(provider, self.headers),
            thinking_token_budget_field: None,
            input_limits,
            prompt_cache,
            sampling_params_by_thinking_level: None,
            model_type: Some(ModelKind::Chat),
            compat,
        })
    }

    /// Convert an `image` entry into a runtime [`ImageModel`] (upstream
    /// `flattenImageModelCatalog`). Cost/context defaults match
    /// [`Self::into_model`]; the api string must be a known [`ImageApi`].
    fn into_image_model(self, provider: &str, provider_base_url: &str) -> AiResult<ImageModel> {
        let id = self.id;
        if id.is_empty() {
            return Err(AiError::Authentication(format!(
                "catalog image model for provider {provider} is missing an id"
            )));
        }
        let input = input_modalities(&self.input);
        let cost = self.cost.unwrap_or_default();
        let api = serde_json::from_value::<crate::ai::types::ImageApi>(serde_json::Value::String(
            self.api.clone(),
        ))
        .map_err(|_| {
            AiError::Authentication(format!(
                "catalog image model {provider}/{id} has unknown image api '{}'",
                self.api
            ))
        })?;
        let base_url = self
            .base_url
            .unwrap_or_else(|| provider_base_url.to_string());
        let mut output = self
            .output
            .unwrap_or_else(|| vec!["image".to_owned()])
            .into_iter()
            .filter_map(|value| {
                serde_json::from_value::<OutputModality>(serde_json::Value::String(value)).ok()
            })
            .collect::<Vec<_>>();
        if output.is_empty() {
            output.push(OutputModality::Image);
        }
        Ok(ImageModel {
            id: id.clone(),
            name: self.name.unwrap_or_else(|| id.clone()),
            api,
            provider: provider.to_string(),
            base_url,
            input,
            cost: ModelCost {
                rates: ModelCostRates {
                    input: cost.input,
                    output: cost.output,
                    cache_read: cost.cache_read,
                    cache_write: cost.cache_write,
                },
                tiers: catalog_tiers(cost.tiers),
            },
            headers: with_static_headers(provider, self.headers),
            output,
        })
    }

    /// Convert a `classifier` entry into a runtime [`ClassifierModel`]
    /// (upstream `flattenClassifierModelCatalog`).
    fn into_classifier_model(
        self,
        provider: &str,
        provider_base_url: &str,
    ) -> AiResult<ClassifierModel> {
        let id = self.id;
        if id.is_empty() {
            return Err(AiError::Authentication(format!(
                "catalog classifier model for provider {provider} is missing an id"
            )));
        }
        let input = input_modalities(&self.input);
        let cost = self.cost.unwrap_or_default();
        let api = serde_json::from_value::<crate::ai::types::ClassifierApi>(
            serde_json::Value::String(self.api.clone()),
        )
        .map_err(|_| {
            AiError::Authentication(format!(
                "catalog classifier model {provider}/{id} has unknown classifier api '{}'",
                self.api
            ))
        })?;
        let base_url = self
            .base_url
            .unwrap_or_else(|| provider_base_url.to_string());
        Ok(ClassifierModel {
            id: id.clone(),
            name: self.name.unwrap_or_else(|| id.clone()),
            api,
            provider: provider.to_string(),
            base_url,
            input,
            cost: ModelCost {
                rates: ModelCostRates {
                    input: cost.input,
                    output: cost.output,
                    cache_read: cost.cache_read,
                    cache_write: cost.cache_write,
                },
                tiers: catalog_tiers(cost.tiers),
            },
            headers: with_static_headers(provider, self.headers),
            context_window: self.context_window.unwrap_or(4096),
        })
    }
}

/// Parse the input-modality list (catalog JSON strings) into the typed enum,
/// defaulting to `["text"]` like the chat path. Takes the field by reference
/// so callers can invoke it after partially moving other `CatalogModel`
/// fields (disjoint borrows).
fn input_modalities(input: &Option<Vec<String>>) -> Vec<InputModality> {
    input
        .clone()
        .unwrap_or_else(|| vec!["text".to_owned()])
        .into_iter()
        .filter_map(|value| serde_json::from_value(serde_json::Value::String(value)).ok())
        .collect()
}

/// Convert the flat catalog cost tiers into runtime [`ModelCostTier`]s.
fn catalog_tiers(tiers: Vec<CatalogCostTier>) -> Vec<ModelCostTier> {
    tiers
        .into_iter()
        .map(|tier| ModelCostTier {
            rates: ModelCostRates {
                input: tier.input,
                output: tier.output,
                cache_read: tier.cache_read,
                cache_write: tier.cache_write,
            },
            input_tokens_above: tier.input_tokens_above,
        })
        .collect()
}

impl CatalogProvider {
    /// Parse one embedded catalog file body.
    pub fn parse(contents: &str) -> AiResult<Self> {
        serde_json::from_str(contents)
            .map_err(|error| AiError::Authentication(format!("invalid provider catalog: {error}")))
    }

    /// The provider display name (id fallback).
    pub fn display_name(&self, provider_id: &str) -> String {
        self.name.clone().unwrap_or_else(|| provider_id.to_string())
    }

    /// The runtime **chat** models, applying the provider-level base URL,
    /// consuming the parsed map. Non-chat entries (`"type": "image"` /
    /// `"type": "classifier"`) are skipped — the chat pipeline must never see
    /// them (upstream keeps the per-type catalogs separate too). Startup parses
    /// one embedded file per provider and immediately converts it; cloning
    /// every `CatalogModel` first (the old `&self` form) duplicated all 1756
    /// catalog entries before conversion. Consuming the map moves each entry
    /// into [`CatalogModel::into_model`] instead.
    pub fn into_models(self, provider_id: &str) -> AiResult<Vec<Model>> {
        let base_url = self.base_url.unwrap_or_default();
        self.models
            .into_iter()
            .filter(|(_, entry)| entry.kind() == ModelKind::Chat)
            .map(|(key, entry)| {
                let mut entry = entry;
                if entry.id.is_empty() {
                    entry.id = key;
                }
                entry.into_model(provider_id, &base_url)
            })
            .collect()
    }

    /// The `image` catalog entries as runtime models (upstream
    /// `flattenImageModelCatalog`). Keys of non-chat entries follow the
    /// upstream `type:id` convention; the entry's `id` field carries the
    /// model id.
    pub fn image_models(&self, provider_id: &str) -> AiResult<Vec<ImageModel>> {
        let base_url = self.base_url.clone().unwrap_or_default();
        self.models
            .iter()
            .filter(|(_, entry)| entry.kind() == ModelKind::Image)
            .map(|(_, entry)| entry.clone().into_image_model(provider_id, &base_url))
            .collect()
    }

    /// The `classifier` catalog entries as runtime models (upstream
    /// `flattenClassifierModelCatalog`).
    pub fn classifier_models(&self, provider_id: &str) -> AiResult<Vec<ClassifierModel>> {
        let base_url = self.base_url.clone().unwrap_or_default();
        self.models
            .iter()
            .filter(|(_, entry)| entry.kind() == ModelKind::Classifier)
            .map(|(_, entry)| entry.clone().into_classifier_model(provider_id, &base_url))
            .collect()
    }
}

/// A provider from the built-in catalog with its parsed runtime models.
#[derive(Debug, Clone)]
pub struct BuiltinProvider {
    pub id: String,
    pub display_name: String,
    pub base_url: Option<String>,
    pub models: Vec<Model>,
}

/// The embedded catalog resources, keyed by provider id (upstream
/// `builtinProviders()` order, subset shipped with pir).
macro_rules! catalog_files {
    ($($id:literal => $file:literal),* $(,)?) => {
        [$(( $id, $file )),*]
    };
}

pub const CATALOG_FILES: &[(&str, &str)] = &catalog_files![
    "amazon-bedrock" => "amazon-bedrock.json",
    "anthropic" => "anthropic.json",
    "ant-ling" => "ant-ling.json",
    "azure" => "azure.json",
    "baseten" => "baseten.json",
    "cerebras" => "cerebras.json",
    "cloudflare-ai-gateway" => "cloudflare-ai-gateway.json",
    "cloudflare-workers-ai" => "cloudflare-workers-ai.json",
    "deepseek" => "deepseek.json",
    "fireworks" => "fireworks.json",
    "github-copilot" => "github-copilot.json",
    "google" => "google.json",
    "google-vertex" => "google-vertex.json",
    "groq" => "groq.json",
    "huggingface" => "huggingface.json",
    "kimi-coding" => "kimi-coding.json",
    "meta" => "meta.json",
    "minimax" => "minimax.json",
    "minimax-cn" => "minimax-cn.json",
    "mistral" => "mistral.json",
    "moonshotai" => "moonshotai.json",
    "moonshotai-cn" => "moonshotai-cn.json",
    "nvidia" => "nvidia.json",
    "openai" => "openai.json",
    "openai-codex" => "openai-codex.json",
    "opencode" => "opencode.json",
    "opencode-go" => "opencode-go.json",
    "openrouter" => "openrouter.json",
    "qwen-token-plan" => "qwen-token-plan.json",
    "qwen-token-plan-cn" => "qwen-token-plan-cn.json",
    "qwen-token-plan-individual" => "qwen-token-plan-individual.json",
    "radius" => "radius.json",
    "together" => "together.json",
    "typesafe" => "typesafe.json",
    "vercel-ai-gateway" => "vercel-ai-gateway.json",
    "xai" => "xai.json",
    "xiaomi" => "xiaomi.json",
    "xiaomi-token-plan-ams" => "xiaomi-token-plan-ams.json",
    "xiaomi-token-plan-cn" => "xiaomi-token-plan-cn.json",
    "xiaomi-token-plan-sgp" => "xiaomi-token-plan-sgp.json",
    "zai" => "zai.json",
    "zai-coding-cn" => "zai-coding-cn.json",
];

include!("catalog/embedded.rs");

/// Date of the last upstream parity refresh of the embedded catalog
/// (upstream `modelDataManifest.generatedAt`, surfaced by
/// `getBuiltinModelDataGeneratedAt` in `providers/all.ts`).
///
/// Upstream writes a full ISO-8601 timestamp (`new Date().toISOString()`);
/// pir pins the timestamp the snapshot was refreshed to. `Date.parse` reads
/// both spellings, and [`CATALOG_GENERATED_AT_MS`] is the same instant.
pub const CATALOG_GENERATED_AT: &str = "2026-10-25T00:00:00.000Z";

/// [`CATALOG_GENERATED_AT`] as Unix milliseconds (`2026-10-25T00:00:00Z`).
pub const CATALOG_GENERATED_AT_MS: u64 = 1_792_886_400_000;

/// Upstream `MODEL_DATA_SCHEMA_VERSION` (`scripts/model-data.ts`): the
/// version of the generated catalog manifest schema.
pub const CATALOG_SCHEMA_VERSION: u32 = 6;

/// Upstream `getBuiltinModelDataGeneratedAt`: the generation timestamp shared
/// by all built-in provider catalogs, in Unix milliseconds.
pub fn builtin_catalog_generated_at() -> Option<u64> {
    Some(CATALOG_GENERATED_AT_MS)
}

/// Deterministic FNV-1a 64 fingerprint of every embedded catalog resource
/// (provider ids, file names, and bodies). Upstream's catalog is identified
/// by its generation manifest; a content digest gives pir's hand-curated
/// snapshot the same stable identity for caches and telemetry.
pub fn builtin_catalog_fingerprint() -> u64 {
    static FINGERPRINT: OnceLock<u64> = OnceLock::new();
    *FINGERPRINT.get_or_init(|| {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = FNV_OFFSET;
        let mut absorb = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            // Field separator, so `ab`+`c` and `a`+`bc` differ.
            hash ^= 0;
            hash = hash.wrapping_mul(FNV_PRIME);
        };
        for (id, file) in CATALOG_FILES {
            absorb(id.as_bytes());
            absorb(file.as_bytes());
            if let Ok(contents) = embedded_catalog(file) {
                absorb(contents.as_bytes());
            }
        }
        hash
    })
}

/// [`builtin_catalog_fingerprint`] as a lowercase hex version string (e.g.
/// for telemetry attributes and cache keys).
pub fn builtin_catalog_version() -> String {
    format!("{:016x}", builtin_catalog_fingerprint())
}

/// The generated catalog's manifest, in the upstream `ModelDataManifest`
/// shape (`scripts/model-data.ts`): `schemaVersion`, `generatedAt`,
/// `structureHash` (SHA-256 over the normalized provider → `type:id` → api
/// map) and per-file SHA-256 hashes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinCatalogManifest {
    /// Upstream `MODEL_DATA_SCHEMA_VERSION`.
    pub schema_version: u32,
    /// Upstream manifest `generatedAt` (ISO-8601).
    pub generated_at: String,
    /// Upstream `modelDataStructureHash`: SHA-256 of the sorted structure.
    pub structure_hash: String,
    /// Upstream manifest `files`: catalog file name → SHA-256 of its bytes.
    pub files: BTreeMap<String, String>,
}

/// The embedded catalog's structure in upstream `ModelDataStructure` shape:
/// provider id → `"<type>:<model id>"` → api id. Chat entries are keyed with
/// the explicit `chat:` prefix upstream's generated data uses (pir omits the
/// per-entry `type` field for chat); image/classifier entries already carry
/// `type:id` keys and are normalized through the same `type:id` identity.
pub fn builtin_catalog_structure() -> AiResult<BTreeMap<String, BTreeMap<String, String>>> {
    let mut structure = BTreeMap::new();
    for (provider_id, file) in CATALOG_FILES {
        let provider = CatalogProvider::parse(embedded_catalog(file)?)?;
        let mut models = BTreeMap::new();
        for (key, entry) in &provider.models {
            let id = if entry.id.is_empty() {
                key.as_str()
            } else {
                entry.id.as_str()
            };
            let identity = format!("{}:{id}", entry.kind().wire_name());
            if models.insert(identity.clone(), entry.api.clone()).is_some() {
                return Err(AiError::Authentication(format!(
                    "catalog provider {provider_id} has duplicate {identity} entries"
                )));
            }
        }
        structure.insert((*provider_id).to_string(), models);
    }
    Ok(structure)
}

/// Upstream `modelDataStructureHash`: SHA-256 (lowercase hex) of the
/// normalized structure JSON (`JSON.stringify(sortedRecord(...))`).
pub fn builtin_catalog_structure_hash() -> AiResult<String> {
    let structure = builtin_catalog_structure()?;
    let json = serde_json::to_string(&structure)?;
    Ok(sha256_hex(json.as_bytes()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn build_builtin_catalog_manifest() -> AiResult<BuiltinCatalogManifest> {
    let structure_hash = builtin_catalog_structure_hash()?;
    let mut files = BTreeMap::new();
    for (_, file) in CATALOG_FILES {
        files.insert(
            (*file).to_string(),
            sha256_hex(embedded_catalog(file)?.as_bytes()),
        );
    }
    Ok(BuiltinCatalogManifest {
        schema_version: CATALOG_SCHEMA_VERSION,
        generated_at: CATALOG_GENERATED_AT.to_string(),
        structure_hash,
        files,
    })
}

/// The embedded catalog's generation manifest (parse-once). This is the
/// public provenance surface for caches, telemetry and the JS
/// `@earendil-works/pi-ai/providers/all` shim (upstream
/// `src/providers/data/.manifest.json`).
pub fn builtin_catalog_manifest() -> AiResult<&'static BuiltinCatalogManifest> {
    static MANIFEST: OnceLock<BuiltinCatalogManifest> = OnceLock::new();
    if let Some(manifest) = MANIFEST.get() {
        return Ok(manifest);
    }
    let manifest = build_builtin_catalog_manifest()?;
    Ok(MANIFEST.get_or_init(|| manifest))
}

/// Parse one embedded provider catalog by id.
pub fn load_provider(provider_id: &str) -> AiResult<Option<BuiltinProvider>> {
    let Some((_, file)) = CATALOG_FILES.iter().find(|(id, _)| *id == provider_id) else {
        return Ok(None);
    };
    let contents = embedded_catalog(file)?;
    let provider = CatalogProvider::parse(contents)?;
    let display_name = provider.display_name(provider_id);
    let base_url = provider.base_url.clone();
    let models = provider.into_models(provider_id)?;
    Ok(Some(BuiltinProvider {
        id: provider_id.to_string(),
        display_name,
        base_url,
        models,
    }))
}

/// [`load_provider`] for the startup composition paths: when the full
/// embedded catalog was already parsed (`load_all_shared`, i.e. every mode
/// host that registers the builtin catalog), look the provider up in that
/// parse-once snapshot instead of re-parsing its JSON file.
///
/// `models.list` / print / TUI compose `models.json` providers ON TOP of the
/// builtin catalog, and the TUI also re-resolves the configured provider. Each
/// of those lookups used to re-parse the embedded file (openrouter.json alone
/// is 244 KB); a `models.json` entry for a builtin provider measured +14 ms on
/// `--list-models` and +16 ms on `models list` at debug opt-level 0.
pub fn load_provider_cached(provider_id: &str) -> AiResult<Option<BuiltinProvider>> {
    if let Some(providers) = PARSED_CATALOG.get() {
        return Ok(providers
            .iter()
            .find(|entry| entry.id == provider_id)
            .cloned());
    }
    load_provider(provider_id)
}

/// Look up one `image` catalog model by provider and id (upstream
/// `getBuiltinImageModel`). `Ok(None)` = unknown provider or id, or the id
/// names a chat model.
pub fn load_image_model(provider_id: &str, model_id: &str) -> AiResult<Option<ImageModel>> {
    let Some((_, file)) = CATALOG_FILES.iter().find(|(id, _)| *id == provider_id) else {
        return Ok(None);
    };
    let provider = CatalogProvider::parse(embedded_catalog(file)?)?;
    Ok(provider
        .image_models(provider_id)?
        .into_iter()
        .find(|model| model.id == model_id))
}

/// Look up one `classifier` catalog model by provider and id (upstream
/// `getBuiltinClassifierModel`).
pub fn load_classifier_model(
    provider_id: &str,
    model_id: &str,
) -> AiResult<Option<ClassifierModel>> {
    let Some((_, file)) = CATALOG_FILES.iter().find(|(id, _)| *id == provider_id) else {
        return Ok(None);
    };
    let provider = CatalogProvider::parse(embedded_catalog(file)?)?;
    Ok(provider
        .classifier_models(provider_id)?
        .into_iter()
        .find(|model| model.id == model_id))
}

/// Parse-once cache of the embedded catalog. The JSON resources are compiled
/// into the binary (`include!`), so parsing is deterministic; the parsed
/// result is computed at most once per process and shared behind an `Arc`
/// (startup-parse-caching item: `load_all` previously re-parsed every embedded
/// provider file on each registry construction). A parse error is not cached —
/// it re-parses and reports the identical error on every call.
static PARSED_CATALOG: OnceLock<Arc<Vec<BuiltinProvider>>> = OnceLock::new();

fn load_all_uncached() -> AiResult<Vec<BuiltinProvider>> {
    CATALOG_FILES
        .iter()
        .map(|(id, _)| load_provider(id))
        .collect::<AiResult<Vec<_>>>()
        .map(|providers| providers.into_iter().flatten().collect())
}

/// All built-in catalog providers as a shared parse-once handle (upstream
/// `getBuiltinProviders`). Read-only callers should prefer this over
/// [`load_all`] to avoid cloning every model entry.
pub fn load_all_shared() -> AiResult<Arc<Vec<BuiltinProvider>>> {
    if let Some(cached) = PARSED_CATALOG.get() {
        return Ok(Arc::clone(cached));
    }
    let providers = load_all_uncached()?;
    Ok(Arc::clone(
        PARSED_CATALOG.get_or_init(|| Arc::new(providers)),
    ))
}

/// All built-in catalog providers (upstream `getBuiltinProviders`). The parse
/// runs at most once per process; each call clones the cached result, which is
/// identical to what re-parsing would produce.
pub fn load_all() -> AiResult<Vec<BuiltinProvider>> {
    Ok(load_all_shared()?.as_ref().clone())
}

/// The api ids pir's protocol adapters can stream in this build. The list is
/// compile-time filtered by the `provider-…` features: an adapter compiled
/// out is absent here, so the registry registers its models for selection and
/// configuration but streaming yields the feature-naming error from
/// [`unsupported_api_error`] instead of constructing a foreign adapter.
pub fn supported_apis() -> &'static [&'static str] {
    &[
        #[cfg(feature = "provider-anthropic")]
        "anthropic-messages",
        #[cfg(feature = "provider-openai")]
        "pi-messages",
        #[cfg(feature = "provider-google")]
        "google-generative-ai",
        #[cfg(feature = "provider-google")]
        "google-vertex",
        #[cfg(feature = "provider-mistral")]
        "mistral-conversations",
        #[cfg(feature = "provider-openai")]
        "openai-completions",
        #[cfg(feature = "provider-openai")]
        "openai-responses",
        #[cfg(feature = "provider-openai")]
        "openai-codex-responses",
        #[cfg(feature = "provider-openai")]
        "azure-openai-responses",
        #[cfg(feature = "provider-bedrock")]
        "bedrock-converse-stream",
    ]
}

/// The `provider-…` feature that compiles the adapter for a wire api id
/// (`None` = no adapter ships for this api in any pir build). The single
/// api→feature table behind [`supported_apis`] and the compiled-out-provider
/// errors.
pub fn api_feature(api: &str) -> Option<&'static str> {
    match api {
        "anthropic-messages" => Some("provider-anthropic"),
        "pi-messages"
        | "openai-completions"
        | "openai-chat"
        | "openai-responses"
        | "openai-codex-responses"
        | "azure-openai-responses" => Some("provider-openai"),
        "google-generative-ai" | "google-vertex" => Some("provider-google"),
        "bedrock-converse-stream" => Some("provider-bedrock"),
        "mistral-conversations" => Some("provider-mistral"),
        _ => None,
    }
}

/// The clear error emitted when streaming a catalog model whose api has no
/// adapter in this build. The model stays listed (the catalog still registers
/// it for selection and configuration); when the api belongs to a compiled-out
/// provider family the message names the disabled feature, and an api with no
/// adapter at all reports the planned-adapter case (upstream has no such gate
/// — its adapters all exist).
pub fn unsupported_api_error(api: &str) -> String {
    match api_feature(api) {
        Some(feature) => format!(
            "the '{api}' API adapter is not available in this build of pir: it was compiled \
             without the `{feature}` feature. The built-in catalog still registers these models \
             for selection and configuration, but streaming requires a build with `{feature}` \
             enabled"
        ),
        None => format!(
            "the '{api}' API adapter is not available in this build of pir; the built-in catalog \
             registers these models for selection and configuration, but streaming requires the \
             dedicated adapter (planned in a later slice)"
        ),
    }
}

/// The honest construction error for an api whose adapter is compiled out:
/// the model cannot be served by another adapter, so the error names the
/// missing feature. Unknown api ids fall back to the OpenAI-compatible
/// adapter when `provider-openai` is enabled, so without it the message names
/// that feature.
pub fn missing_adapter_error(api: &str) -> AiError {
    match api_feature(api) {
        Some(feature) => AiError::MissingProviderAdapter {
            api: api.to_string(),
            feature,
        },
        None => AiError::UnknownProvider(format!(
            "no API adapter for '{api}' in this build of pir (the OpenAI-compatible fallback \
             requires the `provider-openai` feature)"
        )),
    }
}
