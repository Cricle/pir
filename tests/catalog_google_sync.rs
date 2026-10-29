//! Structural checks for the Google / Google Vertex catalog snapshot drift
//! fix (upstream authority: pi `packages/ai/scripts/generate-models.ts`
//! constants + models.dev, 2026-10-09).
//!
//! Self-contained on purpose: it deserializes the two JSON resources with a
//! local mirror of the catalog schema and asserts the synchronized values,
//! so a regression in either file fails loudly here.

use std::collections::BTreeMap;

use serde::Deserialize;

const GOOGLE_JSON: &str = include_str!("../src/ai/providers/catalog/google.json");
const GOOGLE_VERTEX_JSON: &str = include_str!("../src/ai/providers/catalog/google-vertex.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogProvider {
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogModel {
    #[serde(default)]
    id: String,
    api: String,
    #[serde(default)]
    reasoning: bool,
    cost: CatalogCost,
    context_window: u64,
    max_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogCost {
    input: f64,
    output: f64,
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
}

fn google() -> CatalogProvider {
    serde_json::from_str(GOOGLE_JSON).expect("google.json deserializes")
}

fn google_vertex() -> CatalogProvider {
    serde_json::from_str(GOOGLE_VERTEX_JSON).expect("google-vertex.json deserializes")
}

fn assert_shape<'a>(provider: &'a CatalogProvider, id: &str) -> &'a CatalogModel {
    let model = provider
        .models
        .get(id)
        .unwrap_or_else(|| panic!("{id} present"));
    assert_eq!(model.id, id, "{id}: id field matches key");
    assert!(
        model.reasoning,
        "{id}: reasoning models stay flagged as reasoning"
    );
    assert_eq!(model.context_window, 1_048_576, "{id}: context window");
    assert_eq!(model.max_tokens, 65_536, "{id}: max tokens");
    model
}

/// Models must agree on context/maxTokens regardless of provider.
fn assert_aligned(a: &CatalogModel, b: &CatalogModel, id: &str) {
    assert_eq!(a.context_window, b.context_window, "{id}: contextWindow");
    assert_eq!(a.max_tokens, b.max_tokens, "{id}: maxTokens");
}

#[test]
fn google_catalog_parses_and_keeps_authoritative_pricing() {
    let catalog = google();
    assert_eq!(catalog.name.as_deref(), Some("Google"));

    // models.dev `google` + pi generator (source passthrough).
    let flash = assert_shape(&catalog, "gemini-3.5-flash");
    assert_eq!(flash.api, "google-generative-ai");
    assert_eq!(flash.cost.input, 1.5);
    assert_eq!(flash.cost.output, 9.0);
    assert_eq!(flash.cost.cache_read, 0.15);

    let pro = assert_shape(&catalog, "gemini-2.5-pro");
    assert_eq!(pro.api, "google-generative-ai");
    assert_eq!(pro.cost.input, 1.25);
    assert_eq!(pro.cost.output, 10.0);
    assert_eq!(pro.cost.cache_read, 0.125);

    let lite = assert_shape(&catalog, "gemini-3.1-flash-lite");
    assert_eq!(lite.api, "google-generative-ai");
    assert_eq!(lite.cost.input, 0.25);
    assert_eq!(lite.cost.output, 1.5);
    assert_eq!(lite.cost.cache_read, 0.025);

    // Alias rule: gemini-flash-latest inherits gemini-3.5-flash pricing.
    let latest = assert_shape(&catalog, "gemini-flash-latest");
    assert_eq!(latest.api, "google-generative-ai");
    assert_eq!(latest.cost.input, flash.cost.input);
    assert_eq!(latest.cost.output, flash.cost.output);
    assert_eq!(latest.cost.cache_read, flash.cost.cache_read);
}

#[test]
fn google_vertex_catalog_parses_and_matches_google_pricing() {
    let vertex = google_vertex();
    assert_eq!(vertex.name.as_deref(), Some("Google Vertex AI"));
    assert_eq!(
        vertex.base_url.as_deref(),
        Some("https://{location}-aiplatform.googleapis.com")
    );

    let google = google();

    // Same public pricing as the Gemini API; only the api id differs. The
    // generator forces cacheWrite to 0 on Vertex (only cachedContentTokenCount
    // is accounted as cacheRead there).
    for id in [
        "gemini-3.5-flash",
        "gemini-3.1-flash-lite",
        "gemini-flash-latest",
        "gemini-2.5-pro",
    ] {
        let v = vertex
            .models
            .get(id)
            .unwrap_or_else(|| panic!("google-vertex.json contains {id}"));
        let g = google
            .models
            .get(id)
            .unwrap_or_else(|| panic!("google.json contains {id}"));
        assert_shape(&vertex, id);
        assert_eq!(v.api, "google-vertex", "{id}: vertex api id");
        assert_aligned(g, v, id);
        assert_eq!(
            v.cost.input, g.cost.input,
            "{id}: input cost matches google"
        );
        assert_eq!(
            v.cost.output, g.cost.output,
            "{id}: output cost matches google"
        );
        assert_eq!(
            v.cost.cache_read, g.cost.cache_read,
            "{id}: cacheRead matches google"
        );
        assert_eq!(
            v.cost.cache_write, 0.0,
            "{id}: vertex cacheWrite forced to 0"
        );
    }

    // Regression pins for the two drift findings.
    let flash = &vertex.models["gemini-3.5-flash"];
    assert_eq!(flash.cost.input, 1.5, "gemini-3.5-flash input cost");
    assert_eq!(flash.cost.output, 9.0, "gemini-3.5-flash output cost");
    assert_eq!(flash.cost.cache_read, 0.15, "gemini-3.5-flash cacheRead");
    let pro = &vertex.models["gemini-2.5-pro"];
    assert_eq!(pro.cost.cache_read, 0.125, "gemini-2.5-pro cacheRead");
}

#[test]
fn vertex_flash_alias_matches_vertex_flash() {
    let vertex = google_vertex();
    let flash = assert_shape(&vertex, "gemini-3.5-flash");
    let latest = assert_shape(&vertex, "gemini-flash-latest");
    assert_eq!(latest.cost.input, flash.cost.input);
    assert_eq!(latest.cost.output, flash.cost.output);
    assert_eq!(latest.cost.cache_read, flash.cost.cache_read);
}
