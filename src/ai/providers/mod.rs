//! Provider construction from pi's `~/.pi/agent/models.json` configuration
//! plus the built-in provider catalog.
//!
//! Two layers feed the registry, mirroring upstream precedence: the embedded
//! built-in catalog (`catalog` — one JSON resource per provider, upstream
//! ids/display names/base URLs) and user-configured custom endpoints
//! (`pi_config` — `~/.pi/agent/models.json`), whose entries win per provider
//! id via the registry's upsert order. `settings` holds the settings.json
//! snapshot, `config_value` resolves `$ENV`/`!command` credential templates,
//! `models_store` caches refreshed remote catalogs, `resolution` implements
//! the upstream model resolution order, and `construction` holds the
//! protocol-adapter selection shared by both.

use crate::ai::error::AiResult;

pub mod catalog;
pub mod config_value;
pub mod construction;
mod dynamic;
pub mod js_stream;
pub mod models_store;
mod pi_config;
pub mod radius;
pub mod resolution;
mod settings;

pub use construction::CatalogProviderEntry;
pub use dynamic::{
    install_remote_catalog_overlays, remote_catalog_initial_delay, remote_catalog_is_newer,
    DynamicProviderConfig, PersonalizeCatalog, RefreshFn, RegisterDynamicProvider,
    RegisterDynamicProviderShared, RemoteCatalogOverlay, RemoteCatalogOverlayConfig,
    REMOTE_CATALOG_DEFAULT_BASE_URL, REMOTE_CATALOG_MAX_BODY_BYTES,
    REMOTE_CATALOG_REFRESH_INTERVAL, REMOTE_CATALOG_REQUEST_TIMEOUT,
};
pub use js_stream::{
    js_virtual_route_callback, JsStreamProvider, RegisterJsStreamProviderShared,
    JS_ROUTE_HOOK_PREFIX, JS_STREAM_HOOK_PREFIX,
};
pub use models_store::{
    read_models_store_entry, write_models_store_entry, FileModelsStore, InMemoryModelsStore,
    ModelsStore, ModelsStoreEntry,
};
pub use pi_config::{
    has_configured_auth, load_pi_config, load_pi_config_with, load_pi_config_with_settings,
    load_pi_environment, load_pi_environment_with_settings, merge_pi_config, merge_pi_config_in,
    parse_pi_config, strip_jsonc, PiConfiguredModel, PiCostRates, PiEnvironment, PiModelConfig,
    PiModelsConfig, PiProviderConfig,
};
pub use radius::RadiusProvider;
pub use resolution::{
    default_model_per_provider, find_initial_model, resolve_cli_model, resolve_model_scope,
    ScopedModel,
};
pub use settings::{agent_dir, load_settings};

/// Register every built-in catalog provider into the registry (upstream
/// `builtinModels()`). Returns the number of registered providers. models.json
/// entries keep winning: they are registered on top and dynamic/registry
/// lookup consults them through the normal upsert order.
pub fn register_builtin_catalog(
    registry: &mut crate::ai::provider::ProviderRegistry,
) -> AiResult<usize> {
    let providers = catalog::load_all_shared()?;
    let count = providers.len();
    for entry in providers.iter() {
        // Radius ships a baseline catalog but the gateway's account catalog
        // (which can disable models) replaces it via refresh; the provider
        // owns both the baseline and the dynamic state.
        let provider = if entry.id == "radius" {
            RadiusProvider::from_catalog(entry)?
        } else {
            CatalogProviderEntry::from_catalog(entry)?
        };
        registry.set_provider(provider);
    }
    Ok(count)
}
