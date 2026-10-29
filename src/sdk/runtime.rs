//! Session-level SDK factories and lifecycle
//! (upstream `createAgentSession*`, `AgentSessionRuntime`, `core/sdk.ts` /
//! `core/agent-session-runtime.ts`).
//!
//! [`AgentBuilder`](crate::sdk::AgentBuilder) stays the ergonomic single-agent
//! path. These types are the lower-level surface an application that owns
//! session lifecycle needs:
//!
//! - [`CreateAgentSessionOptions`] — the upstream option set (cwd/agent dir,
//!   model registry/model, thinking level, tools, session manager, settings,
//!   resource loader, extension handle);
//! - [`create_agent_session`] — resolve the services and build an
//!   `Arc<AgentSession>`;
//! - [`create_agent_session_services`] /
//!   [`create_agent_session_from_services`] — the split form, for hosts that
//!   resolve settings/resources once and build several sessions from them;
//! - [`AgentSessionRuntime`] — new/switch/reload/dispose over one session;
//! - [`create_agent_session_runtime`] — both, plus the session-directory
//!   binding the runtime's `new_session` needs.
//!
//! These factories are deliberately separate from [`crate::sdk::Agent`]: the
//! `Agent` handle owns streaming/outcome/session-pinning conveniences, while a
//! `CreateAgentSessionResult` is the raw session object (same type the CLI's
//! hosts and the extension API use).
//!
//! Known gaps (not faked):
//!
//! - `createAgentSession`'s "restore the model recorded on an existing
//!   session" step is not repeated here: pass an existing
//!   [`SessionManager`](crate::session_manager::SessionManager) and set the
//!   model explicitly, or use [`crate::sdk::Agent::attach_session`], which
//!   owns the model/thinking restore policy.
//! - JS `newSession({withSession})` / `fork(..., {withSession})` callbacks
//!   run through the host callback bridge (`ExtensionHandle::call_session_callback`);
//!   a RUST embedder drives the equivalent sequence directly:
//!   [`AgentSessionRuntime::new_session`] (or
//!   `AgentSession::replace_session_storage`) followed by its own setup code.
//! - `ctx.reload()` from an EXTENSION COMMAND is applied by the host session
//!   command plane (`AgentSession::reload`); the runtime's
//!   [`AgentSessionRuntime::reload`] is the direct in-process equivalent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::tool_execution::ToolRegistry;
use crate::agent_session::{AgentSession, AgentSessionConfig};
use crate::ai::providers::resolution::DEFAULT_THINKING_LEVEL;
use crate::ai::types::{Model, ThinkingLevel};
use crate::coding_agent::resources::DiscoveredResources;
use crate::compaction::CompactionConfig;
use crate::config::settings::SettingsManager;
use crate::extensions::resources::ExtensionResourceReload;
use crate::session_manager::SessionManager;

use super::error::SdkError;
use super::registry::ModelRegistry;
use super::resources::{DefaultResourceLoader, ResourceLoader};

/// Options for [`create_agent_session`] (upstream
/// `CreateAgentSessionOptions`, `core/sdk.ts:48-106`).
///
/// Every field is optional; the defaults mirror the CLI: process cwd, process
/// agent directory, the settings-resolved default model, threshold compaction,
/// every compiled-in core tool, and a fresh session file under the agent
/// directory's sessions root.
#[derive(Default)]
pub struct CreateAgentSessionOptions {
    /// Working directory for project-local settings/resources and tools.
    pub cwd: Option<PathBuf>,
    /// Global config directory (settings/auth/sessions root). Default: the
    /// process agent directory (`PIR_AGENT_DIR`, else `~/.pi/agent`).
    pub agent_dir: Option<PathBuf>,
    /// Models/credentials. Default: the `models.json` / catalog / `auth.json`
    /// environment of [`Self::agent_dir`] when set, else the process agent
    /// directory's environment.
    pub model_registry: Option<ModelRegistry>,
    /// Explicit model. Default: `settings.defaultProvider` /
    /// `settings.defaultModel`, else the first model with configured auth,
    /// else the first registered model.
    pub model: Option<Model>,
    /// Explicit API key for `model`'s provider.
    pub api_key: Option<String>,
    /// Thinking level. Default: `settings.defaultThinkingLevel`, else
    /// `medium`; clamped to the model's capability by the session.
    pub thinking_level: Option<ThinkingLevel>,
    /// Tool registry. Default: every core tool compiled into this build.
    pub tools: Option<ToolRegistry>,
    /// Start with no tools (`tools` still wins when both are set).
    pub no_tools: bool,
    /// Existing session storage. Default: a fresh session file (or none with
    /// [`Self::no_session`]).
    pub session: Option<SessionManager>,
    /// Keep the transcript in memory (no session file).
    pub no_session: bool,
    /// Automatic compaction. Default: enabled with the upstream defaults;
    /// `Some(CompactionConfig { enabled: false, .. })` disables it.
    /// (`Option` distinguishes "unset" from an explicit `None` is not
    /// possible here; use the `enabled` flag.)
    pub compaction: Option<CompactionConfig>,
    /// Maximum loop turns per prompt. Default: 100.
    pub max_turns: Option<usize>,
    /// Caller-supplied system prompt. Default: the derived prompt.
    pub system_prompt: Option<String>,
    /// JS extension runtime to attach.
    pub extensions: Option<crate::extensions::ExtensionHandle>,
    /// Settings snapshot. Default: loaded for `cwd`/`agent_dir` through the
    /// same trust-aware path every host uses.
    pub settings: Option<SettingsManager>,
    /// Resource loader whose skills/templates install into the session.
    /// `None` installs no prompt resources (the CLI discovers them; a bare
    /// SDK session stays hermetic unless the caller supplies one).
    pub resource_loader: Option<DefaultResourceLoader>,
}

/// Resolved session services (upstream `AgentSessionServices`).
pub struct AgentSessionServices {
    /// Models and configured-auth queries.
    pub model_registry: ModelRegistry,
    /// The merged settings snapshot.
    pub settings: SettingsManager,
    /// Session storage, when the options did not ask for an in-memory session.
    pub session: Option<SessionManager>,
    /// Discovered resources (not yet installed into a session).
    pub resources: DefaultResourceLoader,
}

/// What [`create_agent_session`] produced.
pub struct CreateAgentSessionResult {
    /// The built session (share it; all methods take `&self`).
    pub session: Arc<AgentSession>,
    /// The model the session drives.
    pub model: Model,
    /// The EFFECTIVE thinking level after the model clamp.
    pub thinking_level: ThinkingLevel,
    /// Resources installed into the session (empty when no loader was given).
    pub resources: DiscoveredResources,
    /// Set when the requested/settings model was unavailable and a fallback
    /// was used instead (upstream `modelFallbackMessage`).
    pub model_fallback_message: Option<String>,
}

/// Resolve cwd/agent_dir/settings/registry/storage/resources without building
/// a session (upstream `createAgentSessionServices`).
///
/// The model/thinking/tool/system-prompt fields of `options` are not consumed;
/// pass the same options (a new value) to
/// [`create_agent_session_from_services`].
pub fn create_agent_session_services(
    options: CreateAgentSessionOptions,
) -> Result<AgentSessionServices, SdkError> {
    resolve_services(options).map(|(services, _remainder)| services)
}

/// Build a session from already-resolved services (upstream
/// `createAgentSessionFromServices`).
///
/// The services win for the model registry, settings and stored session; the
/// build fields of `options` (`model`, `thinking_level`, `tools`,
/// `system_prompt`, `extensions`, `api_key`, `compaction`, ...) still apply.
/// A `resource_loader` in `options` replaces the services' loader.
pub async fn create_agent_session_from_services(
    services: AgentSessionServices,
    options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, SdkError> {
    build_from_services(services, options).await
}

/// Build one session from `options` (upstream `createAgentSession`).
pub async fn create_agent_session(
    options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, SdkError> {
    let (services, remainder) = resolve_services(options)?;
    build_from_services(services, remainder).await
}

/// Create one session plus the lifecycle wrapper (upstream
/// `createAgentSessionRuntime`).
pub async fn create_agent_session_runtime(
    options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionRuntimeResult, SdkError> {
    let cwd = resolve_cwd(&options);
    let agent_dir = resolve_agent_dir(&options);
    let no_session = options.no_session;
    let session_dir = if no_session {
        None
    } else {
        Some(default_session_dir(&agent_dir, &cwd))
    };
    let created = create_agent_session(options).await?;
    let runtime =
        AgentSessionRuntime::new(Arc::clone(&created.session), cwd, agent_dir, session_dir);
    Ok(CreateAgentSessionRuntimeResult { runtime, created })
}

/// What [`create_agent_session_runtime`] produced.
pub struct CreateAgentSessionRuntimeResult {
    /// The lifecycle wrapper.
    pub runtime: AgentSessionRuntime,
    /// The creation result (session/model/thinking/resources/fallback).
    pub created: CreateAgentSessionResult,
}

/// Session lifecycle over one [`AgentSession`] (upstream
/// `AgentSessionRuntime`).
///
/// The wrapper does not own the session exclusively: [`Self::session`] hands
/// out the same `Arc` the host may already drive. Every swap refuses to run
/// while a prompt run OR a manual compaction is in flight, and relies on
/// [`AgentSession::replace_session_storage`] so `session_before_switch` plugin
/// handlers can cancel it.
pub struct AgentSessionRuntime {
    session: Arc<AgentSession>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    session_dir: Option<PathBuf>,
}

/// The outcome of [`AgentSessionRuntime::reload`].
#[derive(Debug, Default)]
pub struct AgentSessionReloadOutcome {
    /// `None` when the session has no extension runtime.
    pub extensions: Option<crate::extensions::types::LoadResult>,
    /// Resources the fresh `resources_discover` dispatch installed.
    pub resources: ExtensionResourceReload,
}

impl AgentSessionRuntime {
    /// Wrap an existing session. `session_dir` is the root
    /// [`Self::new_session`] creates files in (`None` = storage-less).
    pub fn new(
        session: Arc<AgentSession>,
        cwd: impl Into<PathBuf>,
        agent_dir: impl Into<PathBuf>,
        session_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            session,
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            session_dir,
        }
    }

    /// The wrapped session.
    pub fn session(&self) -> &Arc<AgentSession> {
        &self.session
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    pub fn session_dir(&self) -> Option<&Path> {
        self.session_dir.as_deref()
    }

    /// Start a fresh session file (upstream `newSession`, without the optional
    /// `withSession` callback — see the module docs).
    pub async fn new_session(&self) -> Result<(), SdkError> {
        self.ensure_idle("new_session")?;
        let Some(session_dir) = self.session_dir.as_ref() else {
            return Err(SdkError::Session(
                "session storage is disabled; new_session requires a persistent session".to_owned(),
            ));
        };
        let manager = SessionManager::create(&self.cwd.to_string_lossy(), session_dir)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        self.session.abort_all_background();
        if self.session.replace_session_storage(manager).await {
            Ok(())
        } else {
            Err(SdkError::Session(
                "the session switch was cancelled by a plugin".to_owned(),
            ))
        }
    }

    /// Switch to an existing session file (upstream `switchSession`, without
    /// the optional `withSession` callback).
    pub async fn switch_session(&self, path: &Path) -> Result<(), SdkError> {
        self.ensure_idle("switch_session")?;
        if self.session.session_manager().is_none() {
            return Err(SdkError::Session(
                "session storage is disabled; switch_session requires a persistent session"
                    .to_owned(),
            ));
        }
        let manager = crate::coding_agent::session_resolve::open_session_tail_first(path)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        self.session.abort_all_background();
        if self.session.replace_session_storage(manager).await {
            Ok(())
        } else {
            Err(SdkError::Session(
                "the session switch was cancelled by a plugin".to_owned(),
            ))
        }
    }

    /// Re-run the extension runtime and its `resources_discover` dispatch
    /// (the working `ctx.reload()` equivalent).
    ///
    /// The JS runtime is rebuilt first (extensions re-execute their factories
    /// exactly once), then the fresh `resources_discover` result merges into
    /// the live resource set. A session without extensions returns an empty
    /// outcome.
    pub async fn reload(&self) -> Result<AgentSessionReloadOutcome, SdkError> {
        self.ensure_idle("reload")?;
        let extensions = match self.session.extensions_handle() {
            Some(handle) => Some(
                handle
                    .reload()
                    .await
                    .map_err(|error| SdkError::Extension(error.to_string()))?,
            ),
            None => None,
        };
        let resources = self.session.reload_extension_resources().await;
        Ok(AgentSessionReloadOutcome {
            extensions,
            resources,
        })
    }

    /// Tear down the extension runtime (upstream `dispose`).
    pub async fn dispose(&self) -> Result<(), SdkError> {
        if let Some(handle) = self.session.extensions_handle() {
            handle
                .shutdown()
                .await
                .map_err(|error| SdkError::Extension(error.to_string()))?;
        }
        Ok(())
    }

    /// Refuse a lifecycle swap while the session is busy. A manual compaction
    /// is not a prompt run (`is_streaming()` stays false) but still owns the
    /// session and appends its summary to the transcript it started against,
    /// so it must block `new_session`/`switch_session`/`reload` exactly like a
    /// run does (the session host's own `/reload` guard uses the same pair).
    fn ensure_idle(&self, operation: &str) -> Result<(), SdkError> {
        if self.session.is_streaming() {
            return Err(SdkError::Other(format!(
                "cannot {operation} while a run is in flight"
            )));
        }
        if self.session.is_compacting() {
            return Err(SdkError::Other(format!(
                "cannot {operation} while a compaction is in flight"
            )));
        }
        Ok(())
    }
}

impl std::fmt::Debug for AgentSessionRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionRuntime")
            .field("cwd", &self.cwd)
            .field("agent_dir", &self.agent_dir)
            .field("session_dir", &self.session_dir)
            .field("streaming", &self.session.is_streaming())
            .finish()
    }
}

fn resolve_cwd(options: &CreateAgentSessionOptions) -> PathBuf {
    options
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn resolve_agent_dir(options: &CreateAgentSessionOptions) -> PathBuf {
    options
        .agent_dir
        .clone()
        .unwrap_or_else(crate::ai::providers::agent_dir)
}

/// `<agent_dir>/sessions/<encoded-cwd>`, the CLI's default session root.
fn default_session_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    crate::session_manager::sessions_dir_in(agent_dir).join(
        crate::session_manager::encode_cwd_dir_name(&crate::session_manager::resolve_path_str(
            &cwd.to_string_lossy(),
        )),
    )
}

/// Resolve the service half of `options`, returning the options with those
/// fields cleared so the build half (model/thinking/tools/...) survives.
fn resolve_services(
    mut options: CreateAgentSessionOptions,
) -> Result<(AgentSessionServices, CreateAgentSessionOptions), SdkError> {
    // Whether the host named its own config directory. The default path keeps
    // the shared environment loader byte-for-byte (including its Claude Code
    // env compatibility); an explicit directory resolves models/auth from
    // THAT directory so sessions are isolated (D2).
    let agent_dir_explicit = options.agent_dir.is_some();
    let cwd = resolve_cwd(&options);
    let agent_dir = resolve_agent_dir(&options);
    let trusted = crate::extensions::project_trust::resolve_project_trust(&cwd, &agent_dir);
    let settings = match options.settings.take() {
        Some(settings) => settings,
        None => SettingsManager::load(&agent_dir, &cwd.join(".pi"), &cwd, trusted),
    };
    let model_registry = match options.model_registry.take() {
        Some(registry) => registry,
        None if agent_dir_explicit => ModelRegistry::from_environment_in(&agent_dir, &cwd)?,
        None => ModelRegistry::from_environment(Some(&cwd))?,
    };
    let resources = match options.resource_loader.take() {
        Some(loader) => loader,
        None => {
            DefaultResourceLoader::new(cwd.clone(), agent_dir.clone()).with_project_trusted(trusted)
        }
    };
    // `session` was already consumed into the services, so the build half
    // must not create a second one.
    let session = match options.session.take() {
        Some(session) => Some(session),
        None if options.no_session => None,
        None => {
            let session_dir = default_session_dir(&agent_dir, &cwd);
            Some(
                SessionManager::create(&cwd.to_string_lossy(), &session_dir)
                    .map_err(|error| SdkError::Session(error.to_string()))?,
            )
        }
    };
    options.cwd = Some(cwd);
    options.agent_dir = Some(agent_dir);
    Ok((
        AgentSessionServices {
            model_registry,
            settings,
            session,
            resources,
        },
        options,
    ))
}

async fn build_from_services(
    services: AgentSessionServices,
    mut options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, SdkError> {
    let cwd = resolve_cwd(&options);
    let agent_dir = resolve_agent_dir(&options);
    let (model, model_fallback_message) = match options.model.take() {
        Some(model) => (model, None),
        None => resolve_model(&services.model_registry, &services.settings)?,
    };
    let thinking = options
        .thinking_level
        .or(services.settings.settings.default_thinking_level)
        .unwrap_or(DEFAULT_THINKING_LEVEL);
    let tools = match options.tools.take() {
        Some(tools) => tools,
        None if options.no_tools => ToolRegistry::new(),
        None => {
            let mut registry = ToolRegistry::new();
            crate::coding_agent::tools::register_core_tools(&mut registry);
            registry
        }
    };
    let session = services.session.or(options.session);
    let extensions = options.extensions.take();
    let host = AgentSession::build_plugin_host_in(
        extensions.as_ref(),
        Vec::new(),
        Some(&cwd),
        Some(&agent_dir),
    );
    // A loader's resources install into the session before the first prompt
    // (`/skill:` expansion and the derived prompt's skills section).
    let mut loader = options.resource_loader.take().unwrap_or(services.resources);
    let resources = loader.reload();
    let provider_registry = Arc::clone(services.model_registry.registry());
    let session_facade = AgentSession::new_arc_with_plugins_and_host_in(
        AgentSessionConfig {
            provider_registry,
            model: model.clone(),
            api_key: options.api_key.take(),
            thinking,
            tools,
            session,
            compaction: Some(options.compaction.take().unwrap_or_default()),
            max_turns: options.max_turns.unwrap_or(100),
            system_prompt: options.system_prompt.take(),
            cwd: Some(cwd),
            event_stream: None,
            extensions,
            tool_selection: None,
        },
        Vec::new(),
        Some(host),
        Some(agent_dir),
    );
    let effective_thinking = session_facade.set_thinking_level(thinking).await;
    if !resources.skills.is_empty() || !resources.prompt_templates.is_empty() {
        session_facade.set_prompt_resources_shared(
            resources.skills.clone(),
            resources.prompt_templates.clone(),
        );
    }
    Ok(CreateAgentSessionResult {
        session: session_facade,
        model,
        thinking_level: effective_thinking,
        resources,
        model_fallback_message,
    })
}

/// Resolve the initial model: settings pair, then the first authenticated
/// model, then the first registered model (upstream `findInitialModel`).
fn resolve_model(
    registry: &ModelRegistry,
    settings: &SettingsManager,
) -> Result<(Model, Option<String>), SdkError> {
    let provider = settings.settings.default_provider.as_deref();
    let model_id = settings.settings.default_model.as_deref();
    if let Some(model_id) = model_id {
        let found = match provider {
            Some(provider) => registry.get_model(provider, model_id),
            None => registry
                .models()
                .into_iter()
                .find(|model| model.id == model_id),
        };
        if let Some(model) = found {
            return Ok((model, None));
        }
        let configured = format!("{}/{}", provider.unwrap_or("<any>"), model_id);
        // A configured-but-unavailable default is a fallback, not a hard
        // failure: upstream reports the fallback and keeps going.
        if let Some(model) = first_available(registry) {
            return Ok((model, Some(format!("Could not restore model {configured}"))));
        }
        return Err(SdkError::NoModel {
            message: format!(
                "the configured default model {configured} is not available and no other model \
                 resolved"
            ),
        });
    }
    first_available(registry)
        .map(|model| (model, None))
        .ok_or_else(|| SdkError::NoModel {
            message: "no model available: configure models.json, credentials, or pass an explicit \
                      model"
                .to_owned(),
        })
}

fn first_available(registry: &ModelRegistry) -> Option<Model> {
    registry
        .available_models()
        .into_iter()
        .next()
        .or_else(|| registry.models().into_iter().next())
}
