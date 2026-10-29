//! [`ModelRegistry`]: the SDK's read face over the provider registry
//! (upstream `ModelRegistry`, `core/model-registry.ts`).
//!
//! Upstream's registry owns model composition, auth resolution and dynamic
//! provider registration. pir already has that machinery in
//! [`crate::ai::provider::ProviderRegistry`] plus the environment loaders, so
//! this type is a thin, owned wrapper — it adds the two questions an embedder
//! asks most (`get_model`, `available_models`) and the configured-auth filter
//! that upstream exposes as `hasConfiguredAuth`, without duplicating provider
//! composition.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::ai::provider::ProviderRegistry;
use crate::ai::types::Model;

pub use crate::ai::providers::PiEnvironment;

use super::error::SdkError;

/// Models and credentials available to an SDK session.
///
/// Two construction paths:
///
/// - [`ModelRegistry::from_environment`] — the full upstream resolution
///   (`models.json`, the built-in catalog, env/bootstrap defaults and
///   `auth.json`), with the configured API keys kept for
///   [`ModelRegistry::available_models`];
/// - [`ModelRegistry::from_registry`] — wrap a caller-supplied registry
///   (the hermetic path used by [`crate::sdk::AgentBuilder::with_registry`]).
///   Configured auth is then probed from `auth.json` / provider environment
///   variables only.
pub struct ModelRegistry {
    registry: Arc<ProviderRegistry>,
    api_keys: BTreeMap<String, String>,
    /// Explicit agent directory when this registry was built for one; scopes
    /// the stored-credential (`auth.json`) probe to that directory. `None`
    /// keeps the process-wide default storage.
    agent_dir: Option<std::path::PathBuf>,
}

impl Clone for ModelRegistry {
    fn clone(&self) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            api_keys: self.api_keys.clone(),
            agent_dir: self.agent_dir.clone(),
        }
    }
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRegistry")
            .field("providers", &self.registry.providers().len())
            .field("models", &self.registry.get_models().len())
            .field("agent_dir", &self.agent_dir)
            .finish()
    }
}

impl ModelRegistry {
    /// Resolve the model environment for `cwd` from the process configuration
    /// (upstream `ModelRuntime.create` with the default paths).
    ///
    /// The `models.json` / `auth.json` directory is the process agent
    /// directory (`PIR_AGENT_DIR`, else `~/.pi/agent`). When the host owns an
    /// explicit agent directory, use [`ModelRegistry::from_environment_in`]
    /// instead so this registry reads THAT directory's configuration.
    pub fn from_environment(cwd: Option<&Path>) -> Result<Self, SdkError> {
        let environment = crate::ai::providers::load_pi_environment(cwd)?;
        Ok(Self {
            registry: Arc::new(environment.registry),
            api_keys: environment.api_keys,
            agent_dir: None,
        })
    }

    /// Resolve the model environment from an EXPLICIT agent directory
    /// (`<agent_dir>/models.json`, `<agent_dir>/settings.json`,
    /// `<agent_dir>/auth.json`) for `cwd`, without consulting the process
    /// `PIR_AGENT_DIR` / home directory.
    ///
    /// This is the runtime [`CreateAgentSessionOptions::agent_dir`] path:
    /// two sessions built around different directories in one process see
    /// their own model configuration, never each other's.
    pub fn from_environment_in(agent_dir: &Path, cwd: &Path) -> Result<Self, SdkError> {
        let environment = super::builder::load_environment_at(agent_dir, cwd)?;
        Ok(Self {
            registry: Arc::new(environment.registry),
            api_keys: environment.api_keys,
            agent_dir: Some(agent_dir.to_path_buf()),
        })
    }

    /// Wrap an existing registry (no configured-key snapshot).
    pub fn from_registry(registry: Arc<ProviderRegistry>) -> Self {
        Self {
            registry,
            api_keys: BTreeMap::new(),
            agent_dir: None,
        }
    }

    /// The underlying provider registry, for stream/registration calls.
    pub fn registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    /// One model by provider id + model id (upstream `getModel`).
    /// Extension/virtual registrations resolve too.
    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        self.registry.get_model(provider_id, model_id)
    }

    /// Every model the current provider set exposes.
    pub fn models(&self) -> Vec<Model> {
        self.registry.get_models()
    }

    /// Models whose provider has a configured credential (upstream
    /// `getAvailable`): a `models.json` key, a provider environment variable,
    /// or a stored `auth.json` credential.
    pub fn available_models(&self) -> Vec<Model> {
        self.registry
            .get_models()
            .into_iter()
            .filter(|model| self.has_configured_auth(&model.provider))
            .collect()
    }

    /// Whether `provider_id` has a usable credential (upstream
    /// `hasConfiguredAuth`).
    pub fn has_configured_auth(&self, provider_id: &str) -> bool {
        match &self.agent_dir {
            Some(agent_dir) => {
                super::builder::has_configured_auth_in(agent_dir, provider_id, &self.api_keys)
            }
            None => crate::ai::providers::has_configured_auth(provider_id, &self.api_keys),
        }
    }

    /// The `models.json`-resolved API key for a provider, when the
    /// environment supplied one. Returned by value because the caller needs
    /// it for a request, not for display; callers must not log it.
    pub fn configured_api_key(&self, provider_id: &str) -> Option<String> {
        self.api_keys.get(provider_id).cloned()
    }
}

impl From<PiEnvironment> for ModelRegistry {
    fn from(environment: PiEnvironment) -> Self {
        Self {
            registry: Arc::new(environment.registry),
            api_keys: environment.api_keys,
            agent_dir: None,
        }
    }
}
