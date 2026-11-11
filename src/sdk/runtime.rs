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
//! - Upstream `CreateAgentSessionOptions.sessionStartEvent` is not exposed.
//!   The raw factory deliberately does not classify session lifecycle
//!   reasons: its first prompt dispatches `session_start {reason:"startup"}`.
//!   Hosts that resume/fork should use [`crate::sdk::Agent::attach_session`] /
//!   [`crate::sdk::Agent::new_session`] (or the runtime switch/fork), which
//!   emit `resume`/`new`/`fork` with `previousSessionFile`.

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

use super::builder::SystemPromptFiles;
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
    /// Home directory the default environment resource loader resolves `~`
    /// and the user-global `.agents/skills` scan against. Default: the process
    /// home (`HOME`, else `USERPROFILE`). Hermetic hosts/tests set this
    /// instead of mutating process environment; an explicit `resource_loader`
    /// owns its home through [`DefaultResourceLoader::with_home`]. This is an
    /// SDK host affordance (upstream `createAgentSession` always uses the
    /// process home); it changes nothing else about the resolved services.
    pub home: Option<PathBuf>,
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
    ///
    /// When set, it wins over every prompt file: no `SYSTEM.md` /
    /// `APPEND_SYSTEM.md` discovery and no `PIR_SYSTEM_PROMPT_FILE` read runs.
    pub system_prompt: Option<String>,
    /// Enable or disable ambient `SYSTEM.md` / `APPEND_SYSTEM.md` discovery
    /// (and the `PIR_SYSTEM_PROMPT_FILE` env file) for the derived prompt.
    ///
    /// Default: enabled (`Some(true)` semantics) when no explicit
    /// `system_prompt` is set, so the raw factory matches the CLI and
    /// [`crate::sdk::AgentBuilder`]'s environment-resolved path. `Some(false)`
    /// keeps the session hermetic. Supplying `system_prompt_files` turns
    /// ambient discovery off unless this is explicitly `Some(true)`.
    pub discover_system_prompt: Option<bool>,
    /// Explicit prompt files applied to the derived prompt. Off by default.
    /// An explicit `system_prompt` still wins; see
    /// [`crate::sdk::SystemPromptFiles`] for the precedence.
    pub system_prompt_files: Option<SystemPromptFiles>,
    /// JS extension runtime to attach.
    pub extensions: Option<crate::extensions::ExtensionHandle>,
    /// Settings snapshot. Default: loaded for `cwd`/`agent_dir` through the
    /// same trust-aware path every host uses.
    pub settings: Option<SettingsManager>,
    /// Resource loader whose skills/templates install into the session.
    ///
    /// `None` resolves an environment [`DefaultResourceLoader`] for
    /// `cwd`/`agent_dir` and installs what it discovers — the same CLI-parity
    /// behavior as upstream `createAgentSession` (`core/sdk.ts:194-206`), so
    /// a bare low-level session is NOT hermetic: ambient `<agent_dir>/skills`,
    /// project skill roots and the user-global `.agents/skills` scan apply
    /// (subject to trust/flags). Supply a loader (for example one built with
    /// [`DefaultResourceLoader::with_home`] and `without_skills`/
    /// `without_prompt_templates`) when the host needs a pinned or hermetic
    /// resource set. [`crate::sdk::AgentBuilder`] is the hermetic-by-default
    /// path for `endpoint`/`with_registry`/`proxy`.
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

    /// Fork the live session at `entry_id` (upstream `AgentSessionRuntime.fork`,
    /// `agent-session-runtime.ts:262`): delegates to
    /// [`AgentSession::fork_entry`], which emits `session_before_fork` (a
    /// handler may cancel), applies the branch, and emits
    /// `session_start {reason:"fork"}` with `previousSessionFile`.
    ///
    /// `position` is the upstream `before|at` choice; `Before` requires a user
    /// message entry and returns its text as `selected_text`.
    pub async fn fork(
        &self,
        entry_id: &str,
        position: crate::agent_session::ForkPosition,
    ) -> Result<crate::agent_session::ForkEntryOutcome, SdkError> {
        self.ensure_idle("fork")?;
        self.session
            .fork_entry(entry_id, position)
            .await
            .map_err(SdkError::Other)
    }

    /// Import a session JSONL file into this runtime's session directory
    /// (upstream `AgentSessionRuntime.importFromJsonl`,
    /// `agent-session-runtime.ts:359`): copy the file (a unique `-N` suffix
    /// when the destination exists), open it with `cwd_override`, and swap it
    /// in through `replace_session_storage` (emits `session_before_switch`
    /// and `session_start {reason:"resume"}`). Returns `true` when a
    /// `session_before_switch` handler cancelled the import.
    ///
    /// Fails with [`SdkError::Session`] when the input file is missing, the
    /// runtime has no session storage, or the imported session's cwd does not
    /// exist.
    pub async fn import_from_jsonl(
        &self,
        input_path: &Path,
        cwd_override: Option<&str>,
    ) -> Result<bool, SdkError> {
        self.ensure_idle("import_from_jsonl")?;
        let Some(session_dir) = self.session_dir.clone() else {
            return Err(SdkError::Session(
                "session storage is disabled; import_from_jsonl requires a persistent runtime"
                    .to_owned(),
            ));
        };
        if !input_path.is_file() {
            return Err(SdkError::Session(format!(
                "Session import file not found: {}",
                input_path.display()
            )));
        }
        std::fs::create_dir_all(&session_dir)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        let file_name = input_path
            .file_name()
            .ok_or_else(|| SdkError::Session("import path has no file name".to_owned()))?;
        let mut destination = session_dir.join(file_name);
        let source_already_stored = std::fs::canonicalize(input_path)
            .ok()
            .zip(std::fs::canonicalize(&destination).ok())
            .is_some_and(|(source, destination)| source == destination);
        if !source_already_stored {
            let stem = input_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let extension = input_path
                .extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
                .unwrap_or_default();
            let mut suffix = 1;
            while destination.exists() {
                destination = session_dir.join(format!("{stem}-{suffix}{extension}"));
                suffix += 1;
            }
            std::fs::copy(input_path, &destination)
                .map_err(|error| SdkError::Session(error.to_string()))?;
        }
        let manager = SessionManager::open_with(&destination, Some(&session_dir), cwd_override)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        let cwd = PathBuf::from(manager.cwd());
        if !cwd.is_dir() {
            return Err(SdkError::Session(format!(
                "imported session cwd does not exist: {}",
                cwd.display()
            )));
        }
        self.session.abort_all_background();
        Ok(!self.session.replace_session_storage(manager).await)
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
            let loader = DefaultResourceLoader::new(cwd.clone(), agent_dir.clone())
                .with_project_trusted(trusted);
            match options.home.take() {
                Some(home) => loader.with_home(home),
                None => loader,
            }
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

/// Apply the SDK's SYSTEM.md / APPEND_SYSTEM.md override to a freshly built
/// session.
///
/// This is the ONE prompt-file implementation both [`crate::sdk::AgentBuilder`]
/// and the low-level session factories call, so the same workspace cannot
/// produce two different system prompts depending on the entry point.
///
/// Precedence for the preamble: `files.system` >
/// `PIR_SYSTEM_PROMPT_FILE` (only when `discover`) > discovered
/// `<cwd>/.pi/SYSTEM.md` (trust-gated) / `<agent_dir>/SYSTEM.md`. Append
/// sections: discovered `APPEND_SYSTEM.md` first, then `files.append`.
/// A session built with an explicit system prompt ignores all of it (the
/// caller owns the whole prompt).
pub(crate) fn apply_system_prompt_files(
    session: &AgentSession,
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    discover: bool,
    files: &SystemPromptFiles,
) -> Result<(), SdkError> {
    let mut preamble = files.system.as_deref().map(read_prompt_file).transpose()?;
    let mut append: Vec<String> = Vec::new();
    if let Some(path) = files.append.as_deref() {
        append.push(read_prompt_file(path)?);
    }
    if discover {
        // The explicit file wins without touching the ambient chain (and
        // without a failure when the env var names an unreadable path).
        if preamble.is_none() {
            if let Some(path) = std::env::var_os("PIR_SYSTEM_PROMPT_FILE").map(PathBuf::from) {
                preamble = Some(read_prompt_file(&path)?);
            }
        }
        let discovered =
            crate::config::settings::discover_system_md(cwd, agent_dir, project_trusted);
        if preamble.is_none() {
            preamble = discovered.0;
        }
        if let Some(section) = discovered.1 {
            append.insert(0, section);
        }
    }
    session.apply_prompt_overrides(preamble, append);
    Ok(())
}

/// Read a prompt file, stripping a UTF-8 BOM like the settings/context-file
/// readers do. Unlike discovery (which skips unreadable files), an explicit
/// env/file path is a caller request: an I/O failure surfaces.
fn read_prompt_file(path: &Path) -> Result<String, SdkError> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
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
    // D-04: the same shared prompt-file path AgentBuilder uses. An explicit
    // `system_prompt` pins the whole prompt (and skips all file IO); without
    // one, SYSTEM.md/APPEND_SYSTEM.md discovery is on by default so the raw
    // factory matches the CLI hosts. `discover_system_prompt: Some(false)`
    // keeps it hermetic; explicit `system_prompt_files` disable the ambient
    // half unless discovery is also switched on.
    let explicit_system_prompt = options.system_prompt.is_some();
    let project_trusted = services.settings.project_trusted;
    // `.system_prompt_files(..)` is itself the opt-out: even an empty value
    // means the caller owns prompt-file selection.
    let explicit_prompt_files = options.system_prompt_files.is_some();
    let prompt_files = options.system_prompt_files.take().unwrap_or_default();
    let discover_system_prompt = options
        .discover_system_prompt
        .unwrap_or(!explicit_prompt_files);
    let session_facade = AgentSession::new_arc_with_plugins_and_host_in_and_prompt_discovery(
        AgentSessionConfig {
            provider_registry,
            model: model.clone(),
            api_key: options.api_key.take(),
            thinking,
            tools,
            session,
            compaction: Some(options.compaction.take().unwrap_or_default()),
            max_turns: options.max_turns.unwrap_or(100),
            performance: crate::config::settings::PerformanceSettings::from_env(),
            system_prompt: options.system_prompt.take(),
            cwd: Some(cwd.clone()),
            event_stream: None,
            extensions,
            tool_selection: None,
        },
        Vec::new(),
        Some(host),
        Some(agent_dir.clone()),
        // The low-level factory ALWAYS owns a resource loader (the caller's
        // explicit one, else the environment default resolved in
        // `resolve_services`). Upstream `createAgentSession` hands that
        // loader to `AgentSession`, whose prompt skills are exactly the
        // loader's. An independent ambient skill scan here would make an
        // explicit `without_skills()` loader unable to pin the prompt.
        // The loader's resources install below through
        // `set_prompt_resources_shared`, which re-renders the derived prompt.
        false,
    );
    if !explicit_system_prompt && (discover_system_prompt || explicit_prompt_files) {
        apply_system_prompt_files(
            &session_facade,
            &cwd,
            &agent_dir,
            project_trusted,
            discover_system_prompt,
            &prompt_files,
        )?;
    }
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
            // `SdkError::NoModel`'s Display supplies the "no model available:"
            // prefix; keep the detail prefix-free so it is not doubled.
            message: "configure models.json, credentials, or pass an explicit model".to_owned(),
        })
}

fn first_available(registry: &ModelRegistry) -> Option<Model> {
    registry
        .available_models()
        .into_iter()
        .next()
        .or_else(|| registry.models().into_iter().next())
}
