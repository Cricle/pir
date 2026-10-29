//! Moved inline unit tests for the built-in model catalog loader and the
//! coding-agent context (`AGENTS.md`) tool-info extraction.

use pir::agent::tool_execution::ToolRegistry;
use pir::ai::providers::catalog::{
    self, builtin_catalog_fingerprint, builtin_catalog_generated_at, builtin_catalog_manifest,
    builtin_catalog_structure, builtin_catalog_structure_hash, builtin_catalog_version, load_all,
    load_provider, CATALOG_FILES, CATALOG_GENERATED_AT, CATALOG_GENERATED_AT_MS,
    CATALOG_SCHEMA_VERSION,
};
use pir::coding_agent::{context, tools};
use serde_json::json;

// ---------------------------------------------------------------------------
// providers/catalog.rs
// ---------------------------------------------------------------------------

#[test]
fn catalog_loads_and_validates() {
    let providers = load_all().unwrap();
    assert_eq!(providers.len(), CATALOG_FILES.len());
    for provider in &providers {
        // TypeSafe registers classifier-only upstream (no chat models); every
        // other provider ships at least one catalog model.
        if provider.id != "typesafe" {
            assert!(!provider.models.is_empty(), "{} has no models", provider.id);
        }
        for model in &provider.models {
            assert_eq!(model.provider, provider.id);
            assert!(!model.api.is_empty());
            assert!(!model.base_url.is_empty() || provider.base_url.is_none());
            assert!(model.context_window > 0, "{} context window", model.id);
            assert!(model.max_tokens > 0, "{} max tokens", model.id);
            assert!(!model.input.is_empty());
            // Cost rates are non-negative.
            assert!(model.cost.rates.input >= 0.0);
            assert!(model.cost.rates.output >= 0.0);
        }
    }
}

#[test]
fn catalog_sample_entries_match_upstream_values() {
    let anthropic = load_provider("anthropic").unwrap().unwrap();
    assert_eq!(anthropic.display_name, "Anthropic");
    let opus = anthropic
        .models
        .iter()
        .find(|model| model.id == "claude-opus-4-8")
        .unwrap();
    assert_eq!(opus.api, "anthropic-messages");
    assert_eq!(opus.base_url, "https://api.anthropic.com");
    assert!(opus.reasoning);
    assert_eq!(opus.input, vec!["text".to_string(), "image".to_string()]);

    let openai = load_provider("openai").unwrap().unwrap();
    let luna = openai
        .models
        .iter()
        .find(|model| model.id == "gpt-5.6-luna")
        .unwrap();
    // Exact upstream generator constants (OPENAI_STANDARD_COSTS).
    assert_eq!(luna.cost.rates.input, 0.2);
    assert_eq!(luna.cost.rates.output, 1.2);
    assert_eq!(luna.cost.rates.cache_read, 0.02);
    assert_eq!(luna.cost.rates.cache_write, 0.25);

    let kimi = load_provider("kimi-coding").unwrap().unwrap();
    let coding = kimi
        .models
        .iter()
        .find(|model| model.id == "kimi-for-coding")
        .unwrap();
    // Exact upstream KIMI_CODING_IMPLIED_COSTS.
    assert_eq!(coding.cost.rates.input, 0.95);
    assert_eq!(coding.cost.rates.output, 4.0);
    assert_eq!(coding.cost.rates.cache_read, 0.19);

    let copilot = load_provider("github-copilot").unwrap().unwrap();
    assert_eq!(
        copilot.base_url.as_deref(),
        Some("https://api.individual.githubcopilot.com")
    );

    let vertex = load_provider("google-vertex").unwrap().unwrap();
    assert_eq!(
        vertex.base_url.as_deref(),
        Some("https://{location}-aiplatform.googleapis.com")
    );
}

#[test]
fn catalog_unknown_provider_is_none() {
    assert!(load_provider("made-up").unwrap().is_none());
}

// ---------------------------------------------------------------------------
// providers/catalog.rs: generation manifest / provenance (audit 5.2 #22)
// ---------------------------------------------------------------------------

#[test]
fn catalog_manifest_matches_the_upstream_wire_shape() {
    let manifest = builtin_catalog_manifest().expect("embedded catalog manifest");
    let value = serde_json::to_value(manifest).expect("serialize manifest");
    let object = value.as_object().expect("manifest object");

    // Upstream `ModelDataManifest` (scripts/model-data.ts) field names.
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        ["schemaVersion", "generatedAt", "structureHash", "files"]
    );
    assert_eq!(
        object["schemaVersion"],
        json!(CATALOG_SCHEMA_VERSION),
        "manifest schemaVersion"
    );
    assert_eq!(object["schemaVersion"], json!(6), "upstream schema version");
    assert_eq!(
        object["generatedAt"],
        json!(CATALOG_GENERATED_AT),
        "manifest generatedAt"
    );
    assert_eq!(
        builtin_catalog_generated_at(),
        Some(CATALOG_GENERATED_AT_MS),
        "getBuiltinModelDataGeneratedAt value"
    );
    assert_eq!(
        CATALOG_GENERATED_AT_MS % 1000,
        0,
        "generatedAt is a whole-second instant"
    );
}

#[test]
fn catalog_structure_uses_upstream_type_id_identities() {
    let structure = builtin_catalog_structure().expect("catalog structure");
    assert_eq!(structure.len(), CATALOG_FILES.len());

    let mut models_seen = 0usize;
    for (provider_id, file) in CATALOG_FILES {
        let parsed: serde_json::Value =
            serde_json::from_str(catalog::embedded_catalog(file).expect("embedded catalog"))
                .expect("catalog JSON");
        let models = parsed["models"].as_object().expect("models object");
        let entries = structure
            .get(*provider_id)
            .unwrap_or_else(|| panic!("{provider_id} missing from structure"));
        assert_eq!(
            entries.len(),
            models.len(),
            "{provider_id} structure/model count"
        );
        for (key, model) in models {
            let kind = model
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("chat");
            let id = model
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .unwrap_or(key);
            let identity = format!("{kind}:{id}");
            assert_eq!(
                entries.get(&identity).map(String::as_str),
                model["api"].as_str(),
                "{provider_id}/{identity} api"
            );
            models_seen += 1;
        }
    }
    assert!(models_seen > 0, "catalog structure is empty");
}

#[test]
fn catalog_structure_hash_and_fingerprint_are_stable_hex() {
    let manifest = builtin_catalog_manifest().expect("embedded catalog manifest");
    let structure_hash = builtin_catalog_structure_hash().expect("structure hash");
    assert_eq!(structure_hash, manifest.structure_hash);
    assert_eq!(structure_hash, builtin_catalog_structure_hash().unwrap());
    assert_eq!(structure_hash.len(), 64, "SHA-256 hex length");
    assert!(
        structure_hash
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()),
        "structureHash is lowercase hex: {structure_hash}"
    );

    let fingerprint = builtin_catalog_fingerprint();
    assert_ne!(fingerprint, 0, "catalog fingerprint");
    assert_eq!(
        builtin_catalog_version(),
        format!("{fingerprint:016x}"),
        "version is the fingerprint in lowercase hex"
    );
    assert_eq!(builtin_catalog_version(), builtin_catalog_version());
}

#[test]
fn catalog_manifest_hashes_every_embedded_resource() {
    let manifest = builtin_catalog_manifest().expect("embedded catalog manifest");
    assert_eq!(manifest.files.len(), CATALOG_FILES.len());

    for (provider_id, file) in CATALOG_FILES {
        let hash = manifest
            .files
            .get(*file)
            .unwrap_or_else(|| panic!("{provider_id}: {file} missing from manifest.files"));
        assert_eq!(hash.len(), 64, "{file} SHA-256 hex length");
        assert!(
            hash.chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()),
            "{file} hash is lowercase hex: {hash}"
        );
        let expected_name = format!("{provider_id}.json");
        assert_eq!(*file, expected_name);
    }
}

/// The manifest JSON pinned in the providers/all shim source.
fn providers_all_pinned_manifest(shim: &str) -> serde_json::Value {
    const MARKER: &str = "const PIR_BUILTIN_MODEL_DATA_MANIFEST_JSON = `";
    let start = shim.find(MARKER).expect("shim manifest constant") + MARKER.len();
    let rest = &shim[start..];
    let end = rest.find('`').expect("shim manifest literal end");
    serde_json::from_str(&rest[..end]).expect("shim manifest JSON")
}

/// The catalog version literal returned by `getBuiltinModelDataVersion()`.
fn providers_all_pinned_version(shim: &str) -> &str {
    const MARKER: &str = "const PIR_BUILTIN_MODEL_DATA_VERSION = \"";
    let start = shim.find(MARKER).expect("shim version constant") + MARKER.len();
    let rest = &shim[start..];
    let end = rest.find('"').expect("shim version literal end");
    &rest[..end]
}

/// The counted form of a manifest/catalog id set: every provider file name
/// maps to exactly one id (the upstream `Object.keys(MODELS)` surface).
fn provider_ids(files: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut ids: Vec<String> = files
        .keys()
        .map(|file| {
            file.strip_suffix(".json")
                .unwrap_or_else(|| panic!("manifest file {file} is not a catalog json resource"))
                .to_string()
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn providers_all_shim_metadata_matches_the_rust_catalog() {
    let shim = include_str!("../../src/extensions/api/js/pi_ai_providers_all_shim.js.txt");
    let manifest = builtin_catalog_manifest().expect("embedded catalog manifest");

    let shim_manifest = providers_all_pinned_manifest(shim);
    assert_eq!(
        shim_manifest,
        serde_json::to_value(manifest).expect("serialize manifest"),
        "providers/all shim manifest must match builtin_catalog_manifest()"
    );
    assert_eq!(
        providers_all_pinned_version(shim),
        builtin_catalog_version(),
        "providers/all shim version must match builtin_catalog_version()"
    );

    assert!(
        shim.contains("export function getBuiltinModelDataGeneratedAt()"),
        "shim must export the upstream generatedAt getter"
    );
    assert!(
        shim.contains("export function getBuiltinModelDataManifest()"),
        "shim must export the catalog manifest"
    );
    // Upstream `getBuiltinProviders()` returns the catalog provider ids
    // (`Object.keys(MODELS)`); the shim derives them from the manifest file
    // names instead of returning an empty placeholder.
    assert!(
        shim.contains("Object.keys(getBuiltinModelDataManifest().files)"),
        "shim getBuiltinProviders must derive ids from the manifest files"
    );
    assert!(
        !shim.contains("export function getBuiltinProviders() { return []; }"),
        "shim getBuiltinProviders must not be an empty placeholder"
    );
}

/// The source-file test above cannot see a stale `assets/shims` binary: the
/// extension host imports the deflate blob, not the `.js.txt`. Inflate the
/// exact `include_bytes!` asset and compare the literals its runtime getters
/// (`getBuiltinModelDataManifest()` / `getBuiltinModelDataVersion()` /
/// `getBuiltinProviders()`) return with the Rust catalog provenance APIs.
#[test]
fn embedded_providers_all_blob_matches_the_rust_catalog() {
    let shim = pir::extensions::api::all_shims()
        .into_iter()
        .find(|(name, _)| *name == "pi_ai_providers_all_shim.js.txt")
        .map(|(_, shim)| shim)
        .expect("providers/all shim is embedded by the extension host");
    let text = shim
        .inflate()
        .expect("inflate the embedded providers/all shim");

    let manifest = builtin_catalog_manifest().expect("embedded catalog manifest");
    let structure_hash = builtin_catalog_structure_hash().expect("structure hash");
    let shim_manifest = providers_all_pinned_manifest(&text);

    assert_eq!(
        shim_manifest,
        serde_json::to_value(manifest).expect("serialize manifest"),
        "embedded blob manifest must match builtin_catalog_manifest()"
    );
    assert_eq!(
        providers_all_pinned_version(&text),
        builtin_catalog_version(),
        "embedded blob version must match builtin_catalog_version()"
    );
    assert_eq!(
        manifest.structure_hash, structure_hash,
        "manifest structureHash must match builtin_catalog_structure_hash()"
    );
    assert_eq!(
        shim_manifest["structureHash"],
        json!(structure_hash),
        "embedded blob structureHash drifted"
    );
    assert_eq!(
        shim_manifest["schemaVersion"],
        json!(CATALOG_SCHEMA_VERSION)
    );
    assert_eq!(shim_manifest["generatedAt"], json!(CATALOG_GENERATED_AT));

    let files = shim_manifest["files"]
        .as_object()
        .expect("manifest files object");
    assert_eq!(
        files.len(),
        42,
        "the shim must pin the 42 built-in provider catalogs"
    );
    assert_eq!(files.len(), CATALOG_FILES.len());
    assert_eq!(
        builtin_catalog_structure()
            .expect("catalog structure")
            .len(),
        42
    );

    let mut rust_ids: Vec<String> = CATALOG_FILES
        .iter()
        .map(|(provider_id, _)| provider_id.to_string())
        .collect();
    rust_ids.sort_unstable();
    assert_eq!(
        provider_ids(files),
        rust_ids,
        "getBuiltinProviders() ids must match CATALOG_FILES"
    );

    for (provider_id, file) in CATALOG_FILES {
        let expected = manifest
            .files
            .get(*file)
            .unwrap_or_else(|| panic!("{provider_id}: {file} missing from manifest.files"));
        assert_eq!(
            files.get(*file).and_then(serde_json::Value::as_str),
            Some(expected.as_str()),
            "{provider_id}: embedded blob file hash drifted"
        );
    }
}

// ---------------------------------------------------------------------------
// coding_agent/context.rs
// ---------------------------------------------------------------------------

#[test]
fn context_tool_infos_extracts_names_and_descriptions() {
    let mut registry = ToolRegistry::new();
    tools::register_core_tools(&mut registry);
    let infos = context::tool_infos(&registry);
    assert!(!infos.is_empty());
    assert!(infos.iter().any(|tool| tool.name == "read"));
    assert!(infos.iter().all(|tool| !tool.description.is_empty()));
}
