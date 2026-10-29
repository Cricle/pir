//! P13: the footer's available-provider count must not materialize the model
//! catalog (once per assistant/usage event).
//!
//! `ProviderRegistry::get_models()` builds a fresh `Vec<Model>` and clones
//! every provider's catalog; the footer only needs the distinct provider
//! count. `distinct_catalog_providers` derives that count from the provider
//! list plus the virtual-model overlay. This test pins it to the exact value
//! the old `get_models()`-based computation produced, so the rendered footer
//! text (which is a pure function of the count, see the `tui_parity_wave`
//! footer fixtures) cannot change.

use std::collections::BTreeSet;
use std::sync::Arc;

use pir::ai::api::openai::OpenAiCompatibleProvider;
use pir::ai::auth::{InMemoryCredentialStore, MapAuthContext};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::ai::virtual_::{PhysicalModelRef, VirtualModelRegistration};
use pir::tui::terminal::distinct_catalog_providers;

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: "openai-completions".into(),
        provider: provider.into(),
        base_url: "https://example.invalid/v1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 32_000,
        max_tokens: 8_192,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn registry() -> ProviderRegistry {
    ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(std::collections::BTreeMap::new())),
    )
}

fn register(registry: &mut ProviderRegistry, provider: &str, models: Vec<Model>) {
    let provider = OpenAiCompatibleProvider::new(provider, "https://example.invalid/v1", models)
        .expect("provider");
    registry.set_provider(Arc::new(provider));
}

fn reference_count(registry: &ProviderRegistry) -> usize {
    registry
        .get_models()
        .iter()
        .map(|model| model.provider.clone())
        .collect::<BTreeSet<_>>()
        .len()
}

#[test]
fn provider_count_matches_the_get_models_union() {
    let mut registry = registry();
    register(
        &mut registry,
        "prov-a",
        vec![model("prov-a", "a-1"), model("prov-a", "a-2")],
    );
    register(&mut registry, "prov-b", vec![model("prov-b", "b-1")]);

    assert_eq!(reference_count(&registry), 2);
    assert_eq!(distinct_catalog_providers(&registry), 2);

    // A virtual model under an EXISTING provider adds no provider to the set.
    registry
        .register_virtual_model(VirtualModelRegistration {
            id: "a-virtual".into(),
            provider: "prov-a".into(),
            name: None,
            physical: PhysicalModelRef {
                provider: "prov-a".into(),
                id: "a-1".into(),
            },
            reasoning: None,
            system_prompt: None,
            model_override: None,
            source: "host".into(),
        })
        .expect("virtual model");
    assert_eq!(reference_count(&registry), 2);
    assert_eq!(distinct_catalog_providers(&registry), 2);

    // A virtual model under a NEW provider id contributes it (get_models
    // appends the overlay entry tagged with `registration.provider`).
    registry
        .register_virtual_model(VirtualModelRegistration {
            id: "virt-1".into(),
            provider: "virt".into(),
            name: None,
            physical: PhysicalModelRef {
                provider: "prov-b".into(),
                id: "b-1".into(),
            },
            reasoning: None,
            system_prompt: None,
            model_override: None,
            source: "host".into(),
        })
        .expect("virtual model");
    assert_eq!(reference_count(&registry), 3);
    assert_eq!(distinct_catalog_providers(&registry), 3);

    // An empty registry has no providers in the catalog (the footer shows no
    // provider prefix). `self::` because the local `registry` binding above
    // shadows the helper of the same name.
    let empty = self::registry();
    assert_eq!(reference_count(&empty), 0);
    assert_eq!(distinct_catalog_providers(&empty), 0);
}
