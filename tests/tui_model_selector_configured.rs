//! S2 `2026-11-06T0415-model-selector-catalog`: the model selector must only
//! list models from providers that are actually configured. The selector
//! header promises "Only showing models from configured providers", and the
//! catalog comes from [`configured_model_pairs`], which applies the same
//! configured-auth semantics as upstream `hasConfiguredAuth` /
//! `ModelRuntime.getAvailableSnapshot()` (models.json key, provider env var,
//! stored auth.json credential).

use std::collections::BTreeMap;

use pir::tui::selector::ModelSelector;
use pir::tui::terminal::configured_model_pairs;

fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(provider, id)| (provider.to_string(), id.to_string()))
        .collect()
}

#[test]
fn configured_model_pairs_keeps_only_the_configured_provider() {
    // The incident's catalog: one configured `mock` provider (models.json
    // API key) and a built-in catalog full of unconfigured providers.
    let models = pairs(&[
        ("mock", "mock-1"),
        ("mistral", "pixtral-large-latest"),
        ("moonshotai", "kimi-k2.6"),
    ]);
    let mut api_keys = BTreeMap::new();
    api_keys.insert("mock".to_string(), "test-key".to_string());

    assert_eq!(
        configured_model_pairs(&models, &api_keys),
        pairs(&[("mock", "mock-1")]),
        "unconfigured providers must not reach the selector"
    );
}

#[test]
fn configured_model_pairs_hides_providers_without_any_credential() {
    // Fake provider ids keep the check independent of the machine's env and
    // auth.json: neither can ever match an unknown provider.
    let models = pairs(&[
        ("zz-unconfigured-a", "model-a"),
        ("zz-unconfigured-b", "model-b"),
    ]);
    assert!(configured_model_pairs(&models, &BTreeMap::new()).is_empty());
}

#[test]
fn configured_selector_renders_only_configured_models() {
    // End of the same pipeline: a selector built from the filtered catalog
    // must not expose the unconfigured rows the incident reported.
    let models = pairs(&[
        ("mock", "mock-1"),
        ("mistral", "pixtral-large-latest"),
        ("moonshotai", "kimi-k2.6"),
    ]);
    let mut api_keys = BTreeMap::new();
    api_keys.insert("mock".to_string(), "test-key".to_string());
    let configured = configured_model_pairs(&models, &api_keys);
    let selector = ModelSelector::new(&configured, Some("mock/mock-1"), None);

    selector.set_query("mistral");
    assert_eq!(
        selector.selected(),
        None,
        "the unconfigured mistral catalog is not searchable"
    );
    selector.set_query("mock-1");
    assert_eq!(selector.selected().as_deref(), Some("mock/mock-1"));
}
