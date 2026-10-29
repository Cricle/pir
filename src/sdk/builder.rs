//! [`AgentBuilder`]: the consuming builder that assembles an [`Agent`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use crate::ai::providers::resolution::InitialModelOptions;
use crate::ai::types::{Model, ThinkingLevel};
use crate::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};
use crate::session_manager::SessionManager;

use super::agent::Agent;
use super::error::SdkError;
use super::resources::{DefaultResourceLoader, ResourceLoader};

/// Wire API of an explicitly configured endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiKind {
    /// OpenAI-compatible chat-completions protocol (upstream
    /// `openai-completions`; covers OpenAI, Ollama, vLLM, OpenRouter, ...).
    OpenAiCompletions,
    /// Anthropic Messages protocol.
    AnthropicMessages,
    /// Google Generative AI protocol.
    GoogleGenerativeAi,
}

impl ApiKind {
    /// The wire api id recorded on the model (upstream models.json `api`).
    pub fn wire_id(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerativeAi => "google-generative-ai",
        }
    }
}

/// An explicitly configured provider endpoint.
///
/// Use this when the target endpoint is known ahead of time (local servers,
/// gateways, custom deployments); the model never consults `models.json` or
/// the built-in catalog on this path.
#[derive(Clone)]
pub struct Endpoint {
    /// Base URL of the API (e.g. `http://localhost:11434/v1`).
    pub base_url: String,
    /// API key sent with requests; `None` for keyless local servers.
    pub api_key: Option<String>,
    /// Wire protocol of the endpoint.
    pub api: ApiKind,
    /// Model id requested on every call.
    pub model_id: String,
    /// Advertised context window (drives threshold compaction).
    pub context_window: u64,
    /// Advised maximum completion tokens.
    pub max_tokens: u64,
}

/// Debug output never prints the API key verbatim: only its presence is
/// shown, so a logged `Endpoint`/`AgentBuilder` cannot leak a credential.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("api", &self.api)
            .field("model_id", &self.model_id)
            .field("context_window", &self.context_window)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

impl Endpoint {
    /// Configure an endpoint with default context limits (128k context,
    /// 8192 completion tokens); adjust the pub fields as needed.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::{ApiKind, Endpoint};
    ///
    /// let endpoint = Endpoint::new("http://localhost:11434/v1", ApiKind::OpenAiCompletions, "llama3");
    /// assert_eq!(endpoint.model_id, "llama3");
    /// assert!(endpoint.api_key.is_none());
    /// ```
    pub fn new(base_url: impl Into<String>, api: ApiKind, model_id: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            api,
            model_id: model_id.into(),
            context_window: 128_000,
            max_tokens: 8_192,
        }
    }
}

/// How the builder obtains the session storage.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum SessionMode {
    /// Persist into the default session directory for the working directory
    /// (upstream `<agent-dir>/sessions/<encoded-cwd>/`).
    #[default]
    DefaultDir,
    /// Persist into an explicit session directory.
    Dir(PathBuf),
    /// Resume an explicit existing session file.
    Existing(PathBuf),
    /// Keep the transcript in memory only; no session file is ever created.
    None,
}

/// An [`AgentTool`] built from an async closure (the ergonomic
/// [`AgentBuilder::tool`] path).
struct ClosureTool<F> {
    name: &'static str,
    description: &'static str,
    parameters: Option<JsonValue>,
    run: F,
}

impl<F, Fut> AgentTool for ClosureTool<F>
where
    F: Fn(JsonValue) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ToolResult> + Send + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> Option<JsonValue> {
        self.parameters.clone()
    }

    fn run(&self, args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin((self.run)(args.clone()))
    }
}

/// What an [`AgentBuilder::on_event`] closure may return: either a full
/// [`PluginEventOutcome`] (e.g. block a `tool_call`) or nothing (a pure
/// observer).
pub trait IntoPluginOutcome {
    #[doc(hidden)]
    fn into_plugin_outcome(self) -> PluginEventOutcome;
}

impl IntoPluginOutcome for () {
    fn into_plugin_outcome(self) -> PluginEventOutcome {
        PluginEventOutcome::None
    }
}

impl IntoPluginOutcome for PluginEventOutcome {
    fn into_plugin_outcome(self) -> PluginEventOutcome {
        self
    }
}

/// An [`AgentPlugin`] built from a single-event closure (the ergonomic
/// [`AgentBuilder::on_event`] path). One instance per `on_event` call, so
/// stacking handlers yields ordered plugin instances exactly like
/// [`AgentBuilder::with_plugin`] attachments.
struct ClosurePlugin<F> {
    info: PluginInfo,
    event_name: &'static str,
    handler: F,
}

impl<F, O> AgentPlugin for ClosurePlugin<F>
where
    F: Fn(&mut JsonValue) -> O + Send + Sync,
    O: IntoPluginOutcome,
{
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![self.event_name.to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        // The host hands every event to every plugin; a closure plugin only
        // answers the event it registered for (its declared `events()`
        // interest), so stacked observers fire once per event, not once per
        // dispatch of any other event.
        if event.name.as_ref() != self.event_name {
            return Box::pin(async move { PluginEventOutcome::None });
        }
        let outcome = (self.handler)(event.payload).into_plugin_outcome();
        Box::pin(async move { outcome })
    }
}

/// Builder for an [`Agent`].
///
/// Consuming setters; chain them and finish with
/// [`AgentBuilder::build`]. Model setup has three shapes:
///
/// 1. explicit [`Endpoint`] (never consults configuration),
/// 2. explicit model id/pattern resolved against the environment
///    (`models.json` + built-in catalog + settings),
/// 3. default resolution (settings defaults, then provider defaults, then
///    the first model with configured auth) — the upstream
///    `findInitialModel` order.
///
/// `AgentBuilder::default()` is exactly [`AgentBuilder::new`].
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> Result<(), pir::sdk::SdkError> {
/// use pir::sdk::AgentBuilder;
///
/// // Explicit endpoint: fully self-contained.
/// let agent = AgentBuilder::new()
///     .endpoint(pir::sdk::Endpoint::new(
///         "http://localhost:11434/v1",
///         pir::sdk::ApiKind::OpenAiCompletions,
///         "llama3",
///     ))
///     .system_prompt("You are a terse assistant.")
///     .no_session()
///     .build()
///     .await?;
///
/// // Environment resolution: reads ~/.pi/agent/models.json, the built-in
/// // catalog, and configured auth, matching the pi CLI.
/// let resolved = AgentBuilder::new().build().await?;
/// # Ok(())
/// # }
/// ```
pub struct AgentBuilder {
    endpoint: Option<Endpoint>,
    /// `(provider, model-id-or-pattern)` resolved against the environment.
    model_pattern: Option<(Option<String>, String)>,
    registry: Option<(Arc<crate::ai::provider::ProviderRegistry>, Model)>,
    /// Tool-selection request (`tools()` allowlist entries); resolved into a
    /// [`crate::coding_agent::ToolSelection`] at build time.
    tool_allowlist: Vec<String>,
    /// Whether [`AgentBuilder::tools`] was called, even with an empty list.
    /// Distinguishes an explicit `.tools([])` ("disable every tool") from the
    /// absence of a selection request (and from an empty `.exclude_tools([])`).
    tool_allowlist_requested: bool,
    /// Tool-selection request (`exclude_tools()` denylist entries).
    tool_denylist: Vec<String>,
    /// Tool-selection request (`no_tools()` switch).
    no_tools: bool,
    /// Whether [`AgentBuilder::tools`] or [`AgentBuilder::no_tools`] was
    /// called. A non-empty denylist is detected from [`Self::tool_denylist`]
    /// itself; an empty `exclude_tools([])` deliberately leaves this false so
    /// it stays a true no-op (upstream `excludeTools: []`).
    tool_selection_requested: bool,
    api_key: Option<String>,
    thinking_level: Option<ThinkingLevel>,
    system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    cwd: Option<PathBuf>,
    /// The agent directory this agent identifies with (D2/D9). `None` resolves
    /// the process environment exactly like the CLI does
    /// ([`crate::ai::providers::agent_dir`]) — the pre-D2 behavior.
    agent_dir: Option<PathBuf>,
    max_turns: Option<usize>,
    tools: ToolRegistry,
    core_tools: bool,
    plugins: Vec<Arc<dyn AgentPlugin>>,
    /// Builtin names requested through [`AgentBuilder::builtin`] /
    /// [`AgentBuilder::builtins`] (bare names, `builtin:` prefix stripped).
    /// Resolved at build time, after the session cwd is known, so the `mcp`
    /// builtin re-roots at it exactly like the CLI's `builtin:<name>` paths.
    builtin_names: Vec<String>,
    /// Diagnostics for requested builtins that cannot be attached in this
    /// build; a non-empty list fails [`AgentBuilder::build`] with the
    /// recorded message instead of silently skipping the plugin.
    builtin_errors: Vec<String>,
    extension_paths: Vec<PathBuf>,
    /// `--only-builtin` embedder equivalent: reduce `extension_paths` to its
    /// `builtin:<name>` entries and never start the QuickJS actor.
    only_builtin: bool,
    session: SessionMode,
    compaction: Option<crate::compaction::CompactionConfig>,
    max_retries: Option<u32>,
    max_retry_delay_ms: Option<u64>,
    /// Per-tool lifecycle hooks installed on the session (upstream
    /// `AgentLoopConfig.beforeToolCall` / `afterToolCall`).
    tool_hooks: Option<Arc<crate::agent::tool_execution::ToolHooks>>,
    /// Optional resource loader whose skills and prompt templates install
    /// into the session before the first run (upstream `ResourceLoader`).
    resource_loader: Option<DefaultResourceLoader>,
}

impl Default for AgentBuilder {
    /// Same configuration as [`AgentBuilder::new`]: environment model
    /// resolution, threshold compaction enabled, and the default session
    /// directory. (`Default` must not silently disable compaction.)
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBuilder {
    /// Create an empty builder: environment model resolution, default
    /// compaction, and the default session directory
    /// ([`SessionMode::DefaultDir`]). Use [`AgentBuilder::no_session`] for an
    /// in-memory transcript.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new();
    /// ```
    pub fn new() -> Self {
        Self {
            endpoint: None,
            model_pattern: None,
            registry: None,
            tool_allowlist: Vec::new(),
            tool_allowlist_requested: false,
            tool_denylist: Vec::new(),
            no_tools: false,
            tool_selection_requested: false,
            api_key: None,
            thinking_level: None,
            system_prompt: None,
            append_system_prompt: None,
            cwd: None,
            agent_dir: None,
            max_turns: None,
            tools: ToolRegistry::default(),
            core_tools: false,
            plugins: Vec::new(),
            builtin_names: Vec::new(),
            builtin_errors: Vec::new(),
            extension_paths: Vec::new(),
            only_builtin: false,
            session: SessionMode::default(),
            compaction: Some(crate::compaction::CompactionConfig::default()),
            max_retries: None,
            max_retry_delay_ms: None,
            tool_hooks: None,
            resource_loader: None,
        }
    }

    /// Configure an explicit endpoint (model setup shape 1).
    ///
    /// The endpoint's [`ApiKind`] selects the wire adapter, so this path
    /// requires the matching `provider-*` cargo feature; a build without it
    /// fails [`AgentBuilder::build`] with an error naming the missing feature.
    /// The endpoint's model is synthesized with `reasoning: false` and no
    /// thinking-level map, so its effective thinking level is always `off`
    /// (`thinking_level(..)` cannot change that — an endpoint carries no
    /// reasoning metadata). Mutually exclusive with [`AgentBuilder::model`],
    /// [`AgentBuilder::model_in_provider`], and
    /// [`AgentBuilder::with_registry`].
    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// Resolve an explicit model from the environment (model setup shape 2):
    /// a bare id, a `provider/model-id` reference, or a fuzzy pattern, with
    /// an optional `:thinking` suffix (e.g. `"openai/gpt-5.5:high"`).
    pub fn model(mut self, pattern: impl Into<String>) -> Self {
        self.model_pattern = Some((None, pattern.into()));
        self
    }

    /// Like [`AgentBuilder::model`], with the provider pinned explicitly.
    pub fn model_in_provider(
        mut self,
        provider: impl Into<String>,
        pattern: impl Into<String>,
    ) -> Self {
        self.model_pattern = Some((Some(provider.into()), pattern.into()));
        self
    }

    /// Supply a fully custom provider registry and model (escape hatch for
    /// custom providers and tests; skips environment resolution entirely).
    ///
    /// This is the only model path that works in a build with no
    /// `provider-*` cargo feature: the [`Provider`](crate::ai::provider::Provider)
    /// implementation in the registry does the streaming, so no wire adapter
    /// is involved. The model is used as supplied, so it must declare a
    /// non-zero `context_window` and `max_tokens`; [`AgentBuilder::build`]
    /// rejects zero values. Mutually exclusive with
    /// [`AgentBuilder::endpoint`] and [`AgentBuilder::model`] /
    /// [`AgentBuilder::model_in_provider`].
    ///
    /// The SDK dispatches this path through
    /// [`ProviderRegistry::stream_unchecked`](crate::ai::provider::ProviderRegistry::stream_unchecked),
    /// so a keyless provider can return an empty
    /// [`api_key_auth`](crate::ai::provider::Provider::api_key_auth) list and
    /// no environment variable is read.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use pir::ai::auth::ApiKeyAuth;
    /// # use pir::ai::event_stream::AssistantMessageEventStream;
    /// # use pir::ai::provider::{Provider, ProviderRegistry};
    /// # use pir::ai::types::{
    /// #     AssistantMessage, Context, Model, ModelCost, ModelCostRates, StopReason,
    /// #     StreamOptions, TerminalReason, Usage,
    /// # };
    /// # use pir::sdk::AgentBuilder;
    /// #
    /// # struct Mock;
    /// #
    /// # impl Provider for Mock {
    /// #     fn id(&self) -> &str { "mock" }
    /// #     fn name(&self) -> &str { "mock" }
    /// #     fn get_models(&self) -> Vec<Model> { vec![model()] }
    /// #     fn api_key_auth(&self) -> Vec<ApiKeyAuth> { Vec::new() }
    /// #     fn stream(
    /// #         &self,
    /// #         _model: &Model,
    /// #         _context: Context,
    /// #         _options: StreamOptions,
    /// #     ) -> AssistantMessageEventStream {
    /// #         let message = AssistantMessage {
    /// #             api: "mock".into(),
    /// #             provider: "mock".into(),
    /// #             model: "mock-model".into(),
    /// #             stop_reason: StopReason::Stop,
    /// #             usage: Usage::default(),
    /// #             ..Default::default()
    /// #         };
    /// #         let stream = AssistantMessageEventStream::new();
    /// #         stream.push(AssistantMessageEventStream::done_event(
    /// #             TerminalReason::Stop,
    /// #             message,
    /// #         ));
    /// #         stream
    /// #     }
    /// # }
    /// #
    /// # fn model() -> Model {
    /// #     Model {
    /// #         id: "mock-model".into(),
    /// #         name: "Mock model".into(),
    /// #         api: "mock-api".into(),
    /// #         provider: "mock".into(),
    /// #         base_url: "http://127.0.0.1:1".into(),
    /// #         reasoning: false,
    /// #         thinking_level_map: None,
    /// #         input: vec!["text".into()],
    /// #         cost: ModelCost {
    /// #             rates: ModelCostRates {
    /// #                 input: 0.0,
    /// #                 output: 0.0,
    /// #                 cache_read: 0.0,
    /// #                 cache_write: 0.0,
    /// #             },
    /// #             tiers: Vec::new(),
    /// #         },
    /// #         context_window: 4_096,
    /// #         max_tokens: 256,
    /// #         sampling_params: None,
    /// #         headers: None,
    /// #         thinking_token_budget_field: None,
    /// #         input_limits: None,
    /// #         prompt_cache: None,
    /// #         sampling_params_by_thinking_level: None,
    /// #         model_type: None,
    /// #         compat: None,
    /// #     }
    /// # }
    /// #
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// let mut registry = ProviderRegistry::default();
    /// registry.set_provider(Arc::new(Mock));
    ///
    /// let agent = AgentBuilder::new()
    ///     .with_registry(Arc::new(registry), model())
    ///     .no_session()
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_registry(
        mut self,
        registry: Arc<crate::ai::provider::ProviderRegistry>,
        model: Model,
    ) -> Self {
        self.registry = Some((registry, model));
        self
    }

    /// Route model requests through an application proxy server — the
    /// upstream `streamProxy` setup (`new Agent({ streamFn: streamProxy })`),
    /// expressed on the provider seam the rest of the SDK uses.
    ///
    /// Registers `provider` in a private registry and pins `model` exactly
    /// like [`AgentBuilder::with_registry`]: no environment/model resolution
    /// runs, and `model` must declare a non-zero `context_window` and
    /// `max_tokens`. The proxy server owns provider credentials
    /// ([`ProxyProvider`](crate::sdk::ProxyProvider) is keyless), and
    /// [`crate::sdk::stream_proxy`] is the direct function analogue when the
    /// caller manages its own registry.
    ///
    /// Mutually exclusive with [`AgentBuilder::endpoint`],
    /// [`AgentBuilder::model`] / [`AgentBuilder::model_in_provider`], and
    /// [`AgentBuilder::with_registry`].
    pub fn proxy(self, provider: super::proxy::ProxyProvider, model: Model) -> Self {
        let mut registry = crate::ai::provider::ProviderRegistry::default();
        registry.set_provider(Arc::new(provider));
        self.with_registry(Arc::new(registry), model)
    }

    /// Explicit API key override. An endpoint configured with
    /// [`Endpoint::api_key`] uses that key (this key is only the fallback for
    /// keyless endpoints); environment-resolved models use this key over the
    /// configured auth.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Requested thinking level (default: the settings default, else
    /// `medium`).
    pub fn thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = Some(level);
        self
    }

    /// Replace the system prompt. Setting it is the only way to suppress the
    /// context-derived prompt (the base preamble/`<tools>`/`<rules>`/`<cwd>`
    /// plus AGENTS.md / CLAUDE.md discovery); when unset, the session always
    /// sends that derived prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Append text to the system prompt (a blank line separates them; upstream
    /// `appendSystemPrompt`). With [`AgentBuilder::system_prompt`] it extends
    /// the explicit prompt; on its own it is the `addendum` appended to the
    /// context-derived prompt (`system-prompt.ts:172`), not a replacement for
    /// it — the `<tools>`/`<rules>`/`<cwd>` sections and context-file
    /// discovery stay in place.
    pub fn append_system_prompt(mut self, text: impl Into<String>) -> Self {
        self.append_system_prompt = Some(text.into());
        self
    }

    /// Working directory for tool executions and project discovery
    /// (default: the process working directory).
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// The agent directory this agent identifies with — the `PIR_AGENT_DIR`
    /// equivalent, as an explicit per-agent identity (D2).
    ///
    /// Every agent-dir-scoped read of THIS build then comes from `dir`
    /// instead of the process environment: `settings.json`, `models.json`,
    /// `auth.json`, the builtins' settings gates, the `mcp` builtin's log and
    /// OAuth store, and (with [`SessionMode::DefaultDir`]) the session root.
    /// The resolved directory is also the key for the process-global caches
    /// the SDK's own layer owns (see "Agent directory identity" in
    /// `docs/sdk.md`), so two agents built around different directories in one
    /// process never share one directory's cached state.
    ///
    /// Unset (the default) resolves the environment exactly like the CLI:
    /// `$PIR_AGENT_DIR`, `$PIR_CODING_AGENT_DIR`, `$PI_CODING_AGENT_DIR`, else
    /// `~/.pi/agent`. Resolution happens once, at [`AgentBuilder::build`]; the
    /// agent keeps that directory for its whole lifetime and a later
    /// environment change never re-points it (D9).
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new().agent_dir("/tmp/agent-b");
    /// ```
    pub fn agent_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.agent_dir = Some(dir.into());
        self
    }

    /// Maximum number of loop turns per prompt (default: 1000 — one turn is
    /// one model request, not one tool call; all calls in a message execute
    /// as one batch).
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// Register a typed [`AgentTool`] implementation.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use pir::agent::tool_batch::ToolResult;
    /// use pir::sdk::AgentBuilder;
    ///
    /// struct Echo;
    /// impl pir::agent::tool_execution::AgentTool for Echo {
    ///     fn name(&self) -> &'static str { "echo" }
    ///     fn description(&self) -> &'static str { "Echoes its input" }
    ///     fn run(&self, args: &serde_json::Value, _ctx: &pir::agent::tool_execution::ToolContext)
    ///         -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send>> {
    ///         let text = args.to_string();
    ///         Box::pin(async move { ToolResult::ok(text) })
    ///     }
    /// }
    ///
    /// let builder = AgentBuilder::new().with_tool(Arc::new(Echo));
    /// ```
    pub fn with_tool(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.register(tool);
        self
    }

    /// Register a tool from an async closure — the ergonomic path for
    /// one-off tools.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use pir::agent::tool_batch::ToolResult;
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new().tool(
    ///     "get_weather",
    ///     "Look up the weather for a city",
    ///     serde_json::json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "city": { "type": "string" }
    ///         },
    ///         "required": ["city"]
    ///     }),
    ///     |args| async move {
    ///         let city = args["city"].as_str().unwrap_or("unknown");
    ///         ToolResult::ok(format!("Weather in {city}: sunny"))
    ///     },
    /// );
    /// ```
    pub fn tool<F, Fut>(
        mut self,
        name: &'static str,
        description: &'static str,
        parameters: JsonValue,
        run: F,
    ) -> Self
    where
        F: Fn(JsonValue) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        self.tools.register(Arc::new(ClosureTool {
            name,
            description,
            parameters: Some(parameters),
            run,
        }));
        self
    }

    /// Register the built-in coding tools (read/write/edit/bash/...).
    /// Disabled by default: the SDK embedder opts in explicitly.
    ///
    /// Requires the aggregate `tools` cargo feature (or at least one
    /// individual `tool-*` leaf). A build that compiled no built-in tool
    /// fails [`AgentBuilder::build`] with the missing-feature diagnostic
    /// instead of silently registering nothing.
    pub fn core_tools(mut self, enabled: bool) -> Self {
        self.core_tools = enabled;
        self
    }

    /// Restrict the tool set to an allowlist (upstream SDK `tools` /
    /// CLI `--tools` semantics). Plain names and `*` patterns
    /// replace the default set; a pure `+name`/`-name` modifier list adds to
    /// / removes from it — the two forms must not be mixed. The default set
    /// is the MERGED registry: everything the builder registered
    /// ([`AgentBuilder::tool`] / [`AgentBuilder::with_tool`], core tools when
    /// [`AgentBuilder::core_tools`] opted in, plus every tool the attached
    /// builtin/plugin/JS-extension surface registers), so `.tools(["subagent"])`
    /// addresses a builtin exactly like a core tool. Modifier and
    /// exclude-only forms start from the tools that activate on registration
    /// (`codemode`/`deferred`/inactive `model-only` tools stay callable but
    /// undeclared unless explicitly named).
    ///
    /// Resolved at build time, after the plugins are instantiated, against
    /// the final merged registry; an invalid list (e.g. mixed forms) fails
    /// [`AgentBuilder::build`] with [`SdkError::Other`]. The selection becomes
    /// the session's active-tool override (F12: the executable registry stays
    /// unfiltered), so a tool left out (or removed by
    /// [`AgentBuilder::exclude_tools`] / [`AgentBuilder::no_tools`]) is not
    /// declared to the model; a `codemode`/`deferred` tool remains callable
    /// through nested tool execution. An explicit empty list (`.tools([])`)
    /// disables every model-facing tool; without any selection call (and
    /// without [`AgentBuilder::exclude_tools`] / [`AgentBuilder::no_tools`])
    /// the exposure defaults decide the active set.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// // Only the `read` core tool is declared to the model.
    /// let builder = AgentBuilder::new().core_tools(true).tools(["read"]);
    ///
    /// // Defaults plus the subagent builtin: modifier form.
    /// let extended = AgentBuilder::new()
    ///     .core_tools(true)
    ///     .builtin("subagent")
    ///     .tools(["+subagent"]);
    ///
    /// // Explicitly disable every tool.
    /// let none = AgentBuilder::new().core_tools(true).tools(Vec::<String>::new());
    /// ```
    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tool_selection_requested = true;
        self.tool_allowlist_requested = true;
        self.tool_allowlist = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Remove tools from the set (upstream SDK `excludeTools` / CLI
    /// `--exclude-tools` semantics): plain names and `*` patterns,
    /// applied on top of the default set (and any
    /// [`AgentBuilder::tools`] allowlist). Entries that match nothing are
    /// ignored, like the CLI flag. An empty list is a true no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new().core_tools(true).exclude_tools(["write", "edit"]);
    /// ```
    pub fn exclude_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        // Do not mark a selection request here: `.exclude_tools([])` must not
        // turn into a static active-set override. A non-empty denylist is
        // recognized by the build step from `tool_denylist` itself.
        self.tool_denylist = tools.into_iter().map(Into::into).collect();
        self
    }

    /// The denylist recorded by [`Self::exclude_tools`].
    ///
    /// Exposed for the `tests/suites/sdk_units.rs` integration suite; not part
    /// of the stable SDK surface.
    #[doc(hidden)]
    #[must_use]
    pub fn tool_denylist(&self) -> &[String] {
        &self.tool_denylist
    }

    /// Whether [`Self::tools`] or [`Self::no_tools`] requested an explicit
    /// tool selection (an empty [`Self::exclude_tools`] list does not).
    ///
    /// Exposed for the `tests/suites/sdk_units.rs` integration suite; not part
    /// of the stable SDK surface.
    #[doc(hidden)]
    #[must_use]
    pub fn tool_selection_requested(&self) -> bool {
        self.tool_selection_requested
    }

    /// Whether threshold compaction is enabled on this builder.
    ///
    /// Exposed for the `tests/suites/sdk_units.rs` integration suite; not part
    /// of the stable SDK surface.
    #[doc(hidden)]
    #[must_use]
    pub fn compaction_enabled(&self) -> bool {
        self.compaction.is_some()
    }

    /// Suppress the default tools (upstream SDK `noTools` / CLI `--no-tools`
    /// semantics). With no explicit [`AgentBuilder::tools`] selection the
    /// model is declared no tools; an explicit list still becomes the active
    /// set, and a pure `+name`/`-name` list applies to the empty default base.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new().core_tools(true).no_tools();
    /// ```
    pub fn no_tools(mut self) -> Self {
        self.tool_selection_requested = true;
        self.no_tools = true;
        self
    }

    /// Register an event handler from a closure — the ergonomic plugin path
    /// for intercepting one unified-dispatch event without writing a full
    /// [`AgentPlugin`](crate::plugins::AgentPlugin) struct. The closure
    /// receives the mutable payload as a [`serde_json::Value`] (upstream
    /// in-place mutation contract; these payloads are the plugin-dispatch
    /// shapes, not [`SdkEvent`](crate::sdk::SdkEvent)s) and may return a
    /// [`PluginEventOutcome`] to act (transform input, override the system
    /// prompt, block a tool call, ...) or `()` to purely observe.
    ///
    /// The closure runs once for every dispatch of `event_name` through the
    /// plugin host (other events are ignored by this handler):
    /// `session_start`, `input`, `before_agent_start`,
    /// `tool_execution_update`, `model_select`, `thinking_level_select`,
    /// `turn_end`, `agent_before_settle`, `session_before_compact`,
    /// `session_before_fork`, `session_before_switch`, ... (the loop-level
    /// `tool_call`/`tool_result` gate currently dispatches through the
    /// JS-extension runtime only — Rust closures observe tool calls via
    /// `tool_execution_update` and the session-level events above).
    ///
    /// Stacking calls yields one plugin instance per handler, dispatched in
    /// registration order (the same ordered merge the plugin host applies to
    /// attached plugins and JS extensions).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use pir::plugins::{InputAction, PluginEventOutcome};
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new()
    ///     // Pure observer: record every submitted prompt.
    ///     .on_event("input", |payload| {
    ///         println!("prompt submitted: {}", payload["text"]);
    ///     })
    ///     // Gatekeeper: rewrite the prompt text before the run (upstream
    ///     // `input` event contract).
    ///     .on_event("input", |payload| {
    ///         let text = payload["text"].as_str().unwrap_or_default().to_owned();
    ///         PluginEventOutcome::Input {
    ///             action: InputAction::Transform,
    ///             text: Some(format!("{text}\n\nAnswer tersely.")),
    ///             images: None,
    ///         }
    ///     })
    ///     // Gatekeeper: override the system prompt for every run.
    ///     .on_event("before_agent_start", |_payload| {
    ///         PluginEventOutcome::BeforeAgentStart {
    ///             system_prompt: Some("You are a terse assistant.".to_owned()),
    ///             message: None,
    ///             messages: Vec::new(),
    ///         }
    ///     });
    /// ```
    pub fn on_event<F, O>(mut self, event_name: &'static str, handler: F) -> Self
    where
        F: Fn(&mut JsonValue) -> O + Send + Sync + 'static,
        O: IntoPluginOutcome,
    {
        self.plugins.push(Arc::new(ClosurePlugin {
            info: PluginInfo::inline("pir-sdk-on-event"),
            event_name,
            handler,
        }));
        self
    }

    /// Attach a Rust plugin ([`AgentPlugin`]) you constructed — the inline
    /// extension path. For a single tool prefer [`AgentBuilder::tool`] /
    /// [`AgentBuilder::with_tool`]; for a builtin this crate ships prefer
    /// [`AgentBuilder::builtin`] (`.builtin("subagent")`). Plugins join the
    /// unified dispatch with JS extensions and can register tools, commands,
    /// and event handlers.
    pub fn with_plugin(mut self, plugin: Arc<dyn AgentPlugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Attach a compiled-in builtin plugin by name — the one-call path for
    /// the plugin families this crate ships: `"subagent"`, `"tool-repair"`,
    /// `"tool-search"`, `"mcp"`, `"python"`, and `"codemode"` (the names
    /// `crate::plugins::builtin_names` lists in the current build).
    ///
    /// Names resolve through the same builtin registry the CLI's
    /// `-e builtin:<name>` path uses
    /// ([`crate::plugins::instantiate_builtin`]) and register exactly like a
    /// [`AgentBuilder::with_plugin`] attachment — one unified registry and
    /// one ordered event dispatch. The bare name (`"subagent"`) and the
    /// `builtin:<name>` form are both accepted.
    ///
    /// # Errors (surfaced by [`AgentBuilder::build`])
    ///
    /// A name that cannot be attached fails the build — never a silent
    /// no-op, matching [`AgentBuilder::extension_paths`]' build-time
    /// failure. A name that is a real builtin this build compiled out
    /// fails with the honest registry diagnostic, e.g. "Built-in extension
    /// `builtin:python` is not compiled into this build (the `python`
    /// feature is off)"; a genuinely unknown name keeps the upstream
    /// wording, "Unknown built-in extension: builtin:<name>". A name the
    /// agent directory's settings disable (`pythonTool: false` /
    /// `disabledBuiltinExtensions`) also fails the build, naming the
    /// setting — the same gates the extension plan honors. Availability
    /// is checked when the name is recorded; the recorded diagnostics are
    /// returned by `build()`, so a typo can never produce an agent that
    /// quietly lacks the plugin.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// // The bare name and the `builtin:` form address the same builtin;
    /// // availability is checked against this build's cargo features.
    /// let builder = AgentBuilder::new()
    ///     .builtin("subagent")
    ///     .builtin("builtin:tool-repair");
    /// ```
    pub fn builtin(mut self, name: impl AsRef<str>) -> Self {
        let name = name.as_ref();
        // `-e builtin:<name>` and the bare name address the same builtin.
        let bare = name
            .strip_prefix(crate::plugins::BUILTIN_PATH_PREFIX)
            .unwrap_or(name);
        // `builtin_extension_def` is the compiled factory list, so its
        // absence is exactly "unknown name, or real builtin with the cargo
        // feature off" — the same fork `missing_builtin_message` diagnoses.
        if crate::plugins::builtin_extension_def(bare).is_some() {
            self.builtin_names.push(bare.to_owned());
        } else {
            self.builtin_errors
                .push(crate::plugins::builtin::missing_builtin_message(bare));
        }
        self
    }

    /// Attach several builtins by name: [`AgentBuilder::builtin`] for each
    /// entry, in order.
    ///
    /// # Examples
    ///
    /// ```
    /// use pir::sdk::AgentBuilder;
    ///
    /// let builder = AgentBuilder::new().builtins(["subagent", "tool-repair"]);
    /// ```
    pub fn builtins<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for name in names {
            self = self.builtin(name);
        }
        self
    }

    /// Load JS extensions from these paths (files or `builtin:<name>`
    /// pseudo-paths, upstream `-e`/`--extension` order).
    ///
    /// Loading is strict: every path is user-supplied, so an on-disk
    /// extension that fails to load or a `builtin:<name>` that does not
    /// resolve to a builtin compiled into this build fails
    /// [`AgentBuilder::build`] (with [`SdkError::Extension`] for load
    /// failures and [`SdkError::Other`] for unresolved builtin names) — never
    /// a silently incomplete agent.
    pub fn extension_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.extension_paths = paths;
        self
    }

    /// The `--only-builtin` policy for embedders: `extension_paths` is
    /// reduced to its `builtin:<name>` entries; on-disk file paths are
    /// ignored with one `[ext]` diagnostic and no QuickJS actor is started
    /// (the plan-only handle still registers every builtin's native tools and
    /// commands). A `builtin:<name>` that does not resolve to a builtin
    /// compiled into this build fails [`AgentBuilder::build`].
    ///
    /// Unlike the CLI flag this does **not** append the default-enabled
    /// builtin set: use [`AgentBuilder::builtin`] / [`AgentBuilder::builtins`]
    /// to attach those explicitly. `only_builtin(false)` (the default) keeps
    /// the supplied paths unchanged.
    pub fn only_builtin(mut self, only_builtin: bool) -> Self {
        self.only_builtin = only_builtin;
        self
    }

    /// Control session persistence (default: [`SessionMode::DefaultDir`]).
    pub fn session(mut self, mode: SessionMode) -> Self {
        self.session = mode;
        self
    }

    /// Shorthand for [`SessionMode::None`] (in-memory transcript).
    pub fn no_session(self) -> Self {
        self.session(SessionMode::None)
    }

    /// Tune threshold compaction; `None` (via
    /// [`AgentBuilder::no_compaction`]) disables automatic compaction.
    pub fn compaction(mut self, config: crate::compaction::CompactionConfig) -> Self {
        self.compaction = Some(config);
        self
    }

    /// Disable automatic (threshold) compaction.
    pub fn no_compaction(mut self) -> Self {
        self.compaction = None;
        self
    }

    /// Tune transport-level retries (`max_retries` attempts, at most
    /// `max_retry_delay_ms` backoff per retry). `None` fields keep the
    /// transport defaults.
    pub fn retry(mut self, max_retries: Option<u32>, max_retry_delay_ms: Option<u64>) -> Self {
        self.max_retries = max_retries;
        self.max_retry_delay_ms = max_retry_delay_ms;
        self
    }

    /// Install the per-tool lifecycle hooks every run uses (upstream
    /// `AgentLoopConfig.beforeToolCall` / `afterToolCall`).
    ///
    /// `before` runs after argument validation and before execution: it may
    /// block the call (with a reason shown in the error result) and mark the
    /// block for batch termination. `after` runs after the tool and before
    /// the call's `tool_execution_end`: it may replace the result's
    /// content/details/error flag/usage and set the termination hint.
    ///
    /// The hooks are handed to the same scheduler that dispatches the
    /// extension/plugin `tool_call` and `tool_result` faces, so those faces
    /// keep their upstream timing and run exactly once. A later
    /// [`Agent::set_tool_hooks`] replaces the installed handle.
    pub fn tool_hooks(mut self, hooks: crate::agent::tool_execution::ToolHooks) -> Self {
        self.tool_hooks = Some(Arc::new(hooks));
        self
    }

    /// [`AgentBuilder::tool_hooks`] with a shared handle (reusable across
    /// agents and replaceable at runtime).
    pub fn shared_tool_hooks(
        mut self,
        hooks: Arc<crate::agent::tool_execution::ToolHooks>,
    ) -> Self {
        self.tool_hooks = Some(hooks);
        self
    }

    /// Supply a [`DefaultResourceLoader`](crate::sdk::DefaultResourceLoader)
    /// whose discovered skills and prompt templates install into the session
    /// before the first run (upstream `ResourceLoader`). This is the
    /// [`AgentBuilder`] form of
    /// [`CreateAgentSessionOptions::resource_loader`](crate::sdk::CreateAgentSessionOptions::resource_loader).
    ///
    /// The SDK stays hermetic by default: no loader means no implicit
    /// discovery (exactly like the lower-level session factories without a
    /// `resource_loader`). A supplied loader is reloaded once during
    /// [`AgentBuilder::build`]; its skills become available to `/skill:`
    /// expansion and the derived system prompt, and its prompt templates are
    /// installed on the session. Configure explicit paths, `--no-skills`
    /// equivalents, home-directory overrides, and project trust on the loader
    /// itself. Loading needs the `skills` cargo feature to actually discover
    /// skills; without it the loader reports the documented disabled
    /// diagnostics and installs nothing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use pir::sdk::{AgentBuilder, DefaultResourceLoader};
    ///
    /// let loader = DefaultResourceLoader::new("/workspace", "/home/me/.pi/agent");
    /// let agent = AgentBuilder::new()
    ///     .resource_loader(loader)
    ///     .no_session()
    ///     .build()
    ///     .await?;
    /// # let _ = agent;
    /// # Ok(())
    /// # }
    /// ```
    pub fn resource_loader(mut self, loader: DefaultResourceLoader) -> Self {
        self.resource_loader = Some(loader);
        self
    }

    /// Assemble the agent.
    ///
    /// Model resolution happens here (never on individual setter calls), so
    /// a failed resolution surfaces as [`SdkError::NoModel`] or
    /// [`SdkError::Provider`].
    ///
    /// Only one model setup may be requested: [`AgentBuilder::endpoint`],
    /// [`AgentBuilder::model`] / [`AgentBuilder::model_in_provider`], and
    /// [`AgentBuilder::with_registry`] are mutually exclusive, and conflicting
    /// combinations fail here. A model supplied through `with_registry` (or an
    /// endpoint) must declare a non-zero `context_window` and `max_tokens`.
    /// A builtin or extension path that cannot be attached also fails here —
    /// never as a silently incomplete agent.
    pub async fn build(self) -> Result<Agent, SdkError> {
        let cwd = self
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd_text = cwd.to_string_lossy().into_owned();
        // D2/D9: the agent's directory identity is resolved EXACTLY ONCE,
        // here — the builder's explicit `.agent_dir(..)` when given, else the
        // process environment (the CLI's resolution). Every agent-dir-scoped
        // read of this build (settings, models.json, auth.json, the builtin
        // gates, the `mcp` builtin root, the session root) and every cache key
        // the SDK's own layer owns uses this value, so two agents built around
        // different directories in one process are isolable and a later
        // environment change never re-points a live agent.
        // Whether the directory is the builder's explicit identity. When it
        // is not, every resolution below takes the shared, environment-backed
        // path byte-for-byte (the pre-D2 behavior).
        let agent_dir_explicit = self.agent_dir.is_some();
        let agent_dir = self
            .agent_dir
            .clone()
            .unwrap_or_else(crate::ai::providers::agent_dir);
        // Explicit setup shapes pin the model/thinking level: a session
        // attached later must not silently replace the caller's choice.
        // Environment/default resolution leaves them unpinned, so
        // `SessionMode::Existing` (and `Agent::attach_session`) can restore
        // the settings recorded on the session branch.
        let model_pinned =
            self.endpoint.is_some() || self.model_pattern.is_some() || self.registry.is_some();
        // An explicit `.thinking_level(..)` pins the level. A strict
        // `:thinking` suffix on `.model(..)` is equally explicit and pins it
        // below once the pattern resolves; without either, an attached
        // session may restore its recorded branch level.
        let mut thinking_pinned = self.thinking_level.is_some();

        // --- model + registry + api key ---------------------------------
        // Model setup shapes are mutually exclusive: a builder carrying more
        // than one of `endpoint`, `model`/`model_in_provider`, and
        // `with_registry` would silently let one win; reject it instead.
        let setups = usize::from(self.endpoint.is_some())
            + usize::from(self.model_pattern.is_some())
            + usize::from(self.registry.is_some());
        if setups > 1 {
            let mut named: Vec<&'static str> = Vec::new();
            if self.endpoint.is_some() {
                named.push("endpoint()");
            }
            if self.model_pattern.is_some() {
                named.push("model()/model_in_provider()");
            }
            if self.registry.is_some() {
                named.push("with_registry()");
            }
            return Err(SdkError::Other(format!(
                "conflicting model configuration: use only one of {} (got {})",
                named.join(", "),
                named.len()
            )));
        }
        // Requested builtins that could not be attached (unknown name, or a
        // real builtin whose cargo feature is off): fail here, before any
        // model/session work, with the diagnostics recorded at the builder
        // call — never hand back an agent silently missing the plugin. This
        // mirrors `extension_paths`, whose failures also surface from
        // `build()`.
        if !self.builtin_errors.is_empty() {
            return Err(SdkError::Other(self.builtin_errors.join("; ")));
        }
        // `core_tools(true)` is a request for the built-in coding tools. A
        // build without the aggregate `tools` feature (or any individual
        // `tool-*` leaf) compiles none of them, so honoring the call by
        // silently registering nothing would hand back a toolless agent the
        // caller did not ask for. Fail with the honest missing-feature
        // diagnostic `.builtin(...)` uses. `CORE_TOOL_NAMES` is the compiled
        // set, so a leaf-only build is not rejected.
        if self.core_tools && crate::coding_agent::tools::CORE_TOOL_NAMES.is_empty() {
            return Err(SdkError::Other(
                "core_tools(true) requires the `tools` cargo feature (or an individual `tool-*` \
                 leaf), but this build compiled no built-in tools (enable `features = [\"tools\"]` \
                 or drop the call)"
                    .to_owned(),
            ));
        }
        let from_endpoint = self.endpoint.is_some();
        // Environment-resolved auth keys, kept for a restored session model
        // whose provider differs from the initially resolved one.
        let mut environment_api_keys: Option<std::collections::BTreeMap<String, String>> = None;
        // Settings `defaultTools`, resolved exactly like
        // [`crate::config::settings::SettingsManager::default_tools`] (a plain
        // list replaces `DEFAULT_TOOL_NAMES`, `+name`/`-name` entries modify
        // it). Only the environment-resolution shape reads settings: an
        // explicit endpoint or `with_registry` build is documented as
        // environment-free, so it stays hermetic.
        let mut settings_default_tools: Option<Vec<String>> = None;
        let (registry, model, resolved_api_key, resolved_thinking) = match self.registry {
            // Escape hatch: caller-supplied registry/model.
            Some((registry, model)) => {
                if model.context_window == 0 || model.max_tokens == 0 {
                    return Err(SdkError::Other(format!(
                        "model `{}` must declare a non-zero context_window and max_tokens",
                        model.id
                    )));
                }
                (registry, model, None, None)
            }
            None => match self.endpoint {
                // Explicit endpoint: register a dynamic provider.
                Some(endpoint) => {
                    if endpoint.context_window == 0 || endpoint.max_tokens == 0 {
                        return Err(SdkError::Other(
                            "endpoint context_window and max_tokens must be non-zero".to_owned(),
                        ));
                    }
                    let api = endpoint.api.wire_id();
                    let stub = crate::ai::providers::construction::model(
                        "sdk-endpoint",
                        &endpoint.model_id,
                        &endpoint.model_id,
                        api,
                        &endpoint.base_url,
                        endpoint.context_window,
                        endpoint.max_tokens,
                        vec!["text"],
                    );
                    let registry = Arc::new(crate::ai::provider::ProviderRegistry::default());
                    crate::ai::providers::RegisterDynamicProviderShared::register_dynamic_provider_shared(
                        registry.as_ref(),
                        "sdk-endpoint",
                        crate::ai::providers::DynamicProviderConfig {
                            base_url: endpoint.base_url,
                            api_key: endpoint.api_key.clone(),
                            api: api.to_owned(),
                            models: vec![stub],
                            compat: None,
                            refresh: None,
                        },
                    )?;
                    let model = registry
                        .get_model("sdk-endpoint", &endpoint.model_id)
                        .ok_or_else(|| SdkError::NoModel {
                            message: format!(
                                "endpoint model `{}` did not register",
                                endpoint.model_id
                            ),
                        })?;
                    (registry, model, endpoint.api_key, None)
                }
                // Environment resolution (models.json + catalog + auth).
                None => {
                    // D2: the pi environment (`settings.json`, `models.json`,
                    // `auth.json`) must come from the agent's OWN directory.
                    // The shared loader resolves the agent dir from the
                    // process environment and takes no directory, so an
                    // explicit `.agent_dir(..)` build resolves the same
                    // inputs from public primitives; the default path keeps
                    // calling the shared loader byte-for-byte.
                    let environment = if agent_dir_explicit {
                        load_environment_at(&agent_dir, cwd.as_path())?
                    } else {
                        crate::ai::providers::load_pi_environment(Some(cwd.as_path()))?
                    };
                    let models = environment.models.clone();
                    let settings = environment.settings.clone();
                    // `has_configured_auth` with the stored-credential read
                    // scoped to this agent's auth.json (identical to the
                    // shared check when the dir IS the environment's).
                    let has_auth = |provider: &str| {
                        has_configured_auth_in(&agent_dir, provider, &environment.api_keys)
                    };
                    // Settings `defaultTools` (upstream `createAgentSession`
                    // seeds the tool selection from
                    // `settingsManager.getDefaultTools()`); an explicit builder
                    // selection still wins below.
                    settings_default_tools = settings
                        .default_tools
                        .as_deref()
                        .map(crate::config::settings::resolve_default_tools);
                    let empty_levels = std::collections::BTreeMap::new();
                    let model_thinking_levels = settings
                        .model_thinking_levels
                        .as_ref()
                        .unwrap_or(&empty_levels);
                    // `enabledModels` is the CLI/session path's scoped-model
                    // list; the SDK applies it only when the caller did not
                    // name a model (a `.model(...)` pattern is explicit and
                    // resolves through `resolve_cli_model`).
                    let scope_patterns = settings.enabled_models.clone().unwrap_or_default();
                    let (scoped_models, scope_diagnostics) =
                        crate::ai::providers::resolution::resolve_model_scope(
                            &scope_patterns,
                            &models,
                        );
                    for diagnostic in &scope_diagnostics {
                        crate::ext_diag!("[sdk] {diagnostic}");
                    }
                    let is_continuing = matches!(self.session, SessionMode::Existing(_));
                    let per_model_level = |model: &Model| {
                        model_thinking_levels
                            .get(&format!("{}/{}", model.provider, model.id))
                            .copied()
                    };
                    let (model, thinking) = match &self.model_pattern {
                        Some((provider, pattern)) => {
                            // Same resolver call the CLI/print host makes
                            // (`resolveCliModel` in upstream `main.ts` passes
                            // `modelRuntime.hasConfiguredAuth`): when one model
                            // id exists on several providers, the one with
                            // configured auth wins instead of an ambiguity
                            // error. An explicit builder key belongs to
                            // whichever provider is selected, so it disables
                            // that tie-break (upstream `--api-key`).
                            let (model, thinking, _warning) =
                                crate::ai::providers::resolution::resolve_cli_model_with_auth(
                                    provider.as_deref(),
                                    pattern,
                                    &models,
                                    &has_auth,
                                    self.api_key.is_some(),
                                    None,
                                )
                                .map_err(|message| SdkError::NoModel { message })?;
                            let model = model.ok_or_else(|| SdkError::NoModel {
                                message: format!("model pattern `{pattern}` resolved to nothing"),
                            })?;
                            // The pattern suffix is the explicit level; the
                            // per-model override and `defaultThinkingLevel`
                            // fill the same chain the print host applies.
                            // A strict `:thinking` suffix is a caller pin:
                            // resume/attach must not replace it with the
                            // branch's recorded level (P2-L).
                            if thinking.is_some() {
                                thinking_pinned = true;
                            }
                            let thinking =
                                Some(crate::agent_session::resolve_starting_thinking_level(
                                    thinking,
                                    per_model_level(&model),
                                    settings.default_thinking_level,
                                ));
                            (model, thinking)
                        }
                        None => {
                            // Same precedence as the CLI/session path
                            // (upstream `findInitialModel`): explicit model,
                            // then scoped (`enabledModels`), then the settings
                            // `defaultProvider`/`defaultModel`, then
                            // provider defaults.
                            let initial = crate::ai::providers::resolution::find_initial_model(
                                InitialModelOptions {
                                    cli_provider: None,
                                    cli_model: None,
                                    cli_thinking: None,
                                    scoped_models: &scoped_models,
                                    is_continuing,
                                    default_provider: settings.default_provider.as_deref(),
                                    default_model_id: settings.default_model.as_deref(),
                                    // `defaultThinkingLevel` never means `off`.
                                    default_thinking_level: settings.default_thinking_level.filter(
                                        |level| *level != crate::ai::types::ThinkingLevel::Off,
                                    ),
                                    model_thinking_levels,
                                    models: &models,
                                    has_auth: &has_auth,
                                    // An explicit builder key goes to whichever
                                    // provider is selected.
                                    inline_api_key: self.api_key.is_some(),
                                },
                            )
                            .map_err(|message| SdkError::NoModel { message })?;
                            let model = initial.model.ok_or_else(|| SdkError::NoModel {
                                message: "no model available; configure a provider with `pir auth login` or ~/.pi/agent/models.json".to_owned(),
                            })?;
                            (model, Some(initial.thinking_level))
                        }
                    };
                    let resolved_api_key = environment.api_keys.get(&model.provider).cloned();
                    environment_api_keys = Some(environment.api_keys.clone());
                    let provider_registry = Arc::new(environment.registry);
                    // Remote catalog overlays (upstream model-runtime
                    // `withRemoteCatalog`): background ETag revalidation,
                    // delayed first hop, PI_OFFLINE-gated. Their
                    // `models-store.json` is a per-agent-dir cache (C12), so
                    // an explicit directory gets its own store instead of
                    // writing into the environment's.
                    if agent_dir_explicit {
                        let _ = install_remote_catalog_overlays_at(&provider_registry, &agent_dir);
                    } else {
                        crate::ai::providers::install_remote_catalog_overlays(&provider_registry);
                    }
                    (provider_registry, model, resolved_api_key, thinking)
                }
            },
        };
        // API key precedence: an explicitly configured endpoint is
        // self-contained, so its key wins and the builder key is only the
        // fallback for keyless endpoints; for environment-resolved models the
        // builder key is the explicit override and configured auth the
        // fallback.
        let mut api_key = if from_endpoint {
            resolved_api_key.or_else(|| self.api_key.clone())
        } else {
            self.api_key.clone().or(resolved_api_key)
        };

        // --- session storage --------------------------------------------
        let session = match &self.session {
            SessionMode::None => None,
            // Tail-first like `Agent::attach_session`: a huge session keeps
            // only its newest page resident (a run widens it before building
            // its context), so opening never parses the whole file.
            SessionMode::Existing(path) => Some(
                crate::coding_agent::session_resolve::open_session_tail_first(path)
                    .map_err(|error| SdkError::Session(error.to_string()))?,
            ),
            SessionMode::Dir(dir) => Some(
                SessionManager::create(&cwd_text, dir)
                    .map_err(|error| SdkError::Session(error.to_string()))?,
            ),
            SessionMode::DefaultDir => Some(
                SessionManager::create(&cwd_text, default_session_dir_in(&agent_dir, &cwd_text))
                    .map_err(|error| SdkError::Session(error.to_string()))?,
            ),
        };

        // --- recorded session settings + totals --------------------------
        // An existing session records the model/thinking level on its branch
        // (upstream `getSessionContextSettings`; the projection walks the
        // leaf→root path, so switches on abandoned branches do not win).
        // Restore them for `SessionMode::Existing` unless the builder pinned
        // either explicitly, and seed the SDK's usage totals from the loaded
        // history. A paged session reports the resident window here; the
        // totals scan below streams its prefix without materializing it.
        //
        // The branch may record a VIRTUAL model (`model_change` appended by
        // `record_virtual_selection`), whose catalog entry only exists once its
        // persisted `pi.virtual-model-state` registration is restored into the
        // registry overlay. That restore therefore runs FIRST — the
        // constructor-time one (`AgentSession::new`) is too late: by then this
        // build has already resolved (or dropped) the recorded selection.
        let declared_virtual = session
            .as_ref()
            .map(|manager| {
                crate::agent_session::restore_virtual_models_from(registry.as_ref(), manager)
            })
            .unwrap_or_default();
        let recorded_settings = session
            .as_ref()
            .and_then(|manager| manager.build_session_projection().ok());
        let mut model = model;
        if !model_pinned {
            if let Some((provider, model_id)) = recorded_settings
                .as_ref()
                .and_then(|projection| projection.model.clone())
            {
                if let Some(restored) = registry.get_model(&provider, &model_id) {
                    // The explicit key belongs to the initially resolved
                    // provider; a cross-provider restore must use that
                    // provider's configured auth instead.
                    if restored.provider != model.provider {
                        // Never carry the builder's explicit key across a
                        // provider boundary: it was resolved for the initial
                        // provider and would be sent as a bearer token to the
                        // restored provider's endpoint. Use only that
                        // provider's own configured/environment credential;
                        // leave it unset otherwise so the normal auth-store
                        // resolution runs.
                        api_key = environment_api_keys
                            .as_ref()
                            .and_then(|keys| keys.get(&restored.provider).cloned());
                    }
                    model = restored;
                } else if declared_virtual
                    .iter()
                    .any(|(p, id)| p == &provider && id == &model_id)
                {
                    // The branch names a virtual model this process cannot
                    // rebuild (its registration entry is gone or malformed):
                    // silently keeping the default would route the resumed
                    // session to the wrong model — fail with the same
                    // resolution error the caller would get from a fresh build.
                    return Err(SdkError::NoModel {
                        message: format!(
                            "session {session} records the virtual model `{provider}/{model_id}`, \
                             which cannot be restored: no `{entry}` registration for it on the \
                             branch (re-install the extension that registered it)",
                            session = session
                                .as_ref()
                                .map(|manager| manager.id().to_owned())
                                .unwrap_or_default(),
                            entry = crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY,
                        ),
                    });
                }
            }
        }
        let initial_usage = session
            .as_ref()
            .map(super::agent::session_history_totals)
            .unwrap_or_default();

        // --- tools -------------------------------------------------------
        // Base tools only at this point. The builder's tool selection is
        // resolved LATER, after the plugin host exists, against the merged
        // registry (base tools + builtin/plugin/extension tools): `.tools(
        // ["subagent"])` must match a builtin, and `.no_tools()` /
        // `.exclude_tools()` must reach tools that merge after this point.
        let mut tools = self.tools;
        if self.core_tools {
            crate::coding_agent::tools::register_core_tools(&mut tools);
        }

        // --- extension runtime (strict) ----------------------------------
        // `only_builtin()` filters the requested paths down to
        // `builtin:<name>` entries before the loader partitions them: an
        // on-disk path can never reach the QuickJS actor, and the resulting
        // plan-only handle still registers every builtin's native surface.
        let extension_paths = if self.only_builtin {
            let (paths, ignored) = crate::plugins::filter_builtin_paths(&self.extension_paths);
            if !ignored.is_empty() {
                let count = ignored.len();
                let ignored = ignored
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                crate::ext_diag!(
                    "[ext] only_builtin ignores {count} on-disk extension path(s): {ignored}"
                );
            }
            paths
        } else {
            self.extension_paths.clone()
        };
        let extensions = if extension_paths.is_empty() {
            None
        } else {
            // The SDK has no package-contributed paths: every requested path
            // is user-supplied, so the STRICT loader applies — an on-disk
            // extension that fails to load or an unknown `builtin:<name>` is a
            // build error, never a silent diagnostic. A `only_builtin()` plan
            // is engine-free by construction (plan-only handle), so it stays
            // valid in a build without the JS engine.
            #[cfg(not(feature = "extensions"))]
            {
                // A no-engine build can still honor `builtin:<name>` entries:
                // they resolve to native plugin factories and the strict
                // loader uses the plan-only (engine-free) handle for a plan
                // with no on-disk path. Only an actual on-disk extension
                // needs QuickJS, so the honest error names exactly those.
                let (_builtin_names, file_paths) =
                    crate::plugins::partition_paths(&extension_paths);
                if !self.only_builtin && !file_paths.is_empty() {
                    let names = file_paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(SdkError::Other(format!(
                        "extension path(s) require the JS extension runtime, but this build has \
                         no JS engine (the `extensions` feature is off): {names}"
                    )));
                }
            }
            // Model resolution already happened above, so the registry is
            // available: push its catalog pre-load (factories may resolve a
            // model synchronously during load).
            #[cfg(feature = "extensions")]
            let model_catalog = Some(crate::agent_session::ModelCatalog::from_registry(&registry));
            #[cfg(not(feature = "extensions"))]
            let model_catalog: Option<crate::agent_session::ModelCatalog> = None;
            let loaded = crate::agent_session::load_extension_runtime_strict_with_catalog(
                &extension_paths,
                crate::extensions::ExtensionConfig {
                    mode: "headless".to_owned(),
                    cwd: cwd.clone(),
                    ..Default::default()
                },
                model_catalog.as_ref(),
            )
            .await;
            match loaded {
                Ok((runtime, failures)) => {
                    if failures.is_empty() {
                        runtime
                    } else {
                        // Split the strict failures: an unresolved
                        // `builtin:<name>` is a configuration error surfaced
                        // as `Other`; an on-disk load failure is an
                        // `Extension` error. Both fail the build.
                        let mut load_messages: Vec<String> = Vec::new();
                        let mut builtin_messages: Vec<String> = Vec::new();
                        for failure in failures {
                            let builtin_failure = failure.path.as_ref().is_some_and(|path| {
                                crate::plugins::is_builtin_path(&path.to_string_lossy())
                            });
                            if builtin_failure {
                                builtin_messages.push(failure.message);
                            } else {
                                load_messages.push(failure.message);
                            }
                        }
                        if !builtin_messages.is_empty() {
                            return Err(SdkError::Other(builtin_messages.join("; ")));
                        }
                        return Err(SdkError::Extension(load_messages.join("; ")));
                    }
                }
                Err(error) => {
                    return Err(SdkError::Extension(format!(
                        "failed to start the extension runtime: {error}"
                    )));
                }
            }
        };

        // --- plugin surface ----------------------------------------------
        // Settings-driven builtin gates apply to `.builtin(...)` too. The
        // extension PLAN is filtered by `build_plugin_host_in` below, but a
        // named builtin is instantiated directly here, so without this check
        // `pythonTool: false` / `disabledBuiltinExtensions` would silently not
        // reach it (the agent-dir section of docs/sdk.md promises they do).
        // Same strict contract as an unknown name: an explicitly requested
        // builtin the settings disable fails the build with the reason, never
        // a silently absent plugin.
        if !self.builtin_names.is_empty() {
            let settings = crate::agent_session::AgentSession::host_settings_in(
                Some(cwd.as_path()),
                Some(agent_dir.as_path()),
            );
            let disabled = settings.disabled_builtins();
            let blocked: Vec<&str> = self
                .builtin_names
                .iter()
                .map(String::as_str)
                .filter(|name| disabled.contains(*name))
                .collect();
            if !blocked.is_empty() {
                let names = blocked
                    .iter()
                    .map(|name| format!("`builtin:{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(SdkError::Other(format!(
                    "built-in extension(s) {names} disabled by settings (`pythonTool: false` or \
                     `disabledBuiltinExtensions`) for agent directory `{}`; remove the \
                     `.builtin(..)` call or enable the builtin in settings",
                    agent_dir.display()
                )));
            }
        }
        // Builtins attached by name resolve HERE, after the session cwd is
        // known: `rewire_builtin_for_session` re-roots the `mcp` builtin at
        // it (the same re-rooting `PluginHost::build` applies to
        // `builtin:<name>` path-plan entries). Explicit plugins keep their
        // attachment order after them.
        let mut user_plugins: Vec<Arc<dyn AgentPlugin>> = Vec::new();
        for name in &self.builtin_names {
            // Total, even though the eager check above already rejected
            // every unattachable name: a builtin request is never dropped
            // silently.
            //
            // C3: the `codemode` builtin's process-wide config cache
            // (`cached_codemode_config`) resolves settings from the PROCESS
            // agent directory. An explicit agent directory picks the
            // per-agent state instead (`CodemodePlugin::with_config`), with
            // its settings read from that directory — no global cache entry
            // is consulted or written for this plugin instance.
            #[cfg(feature = "extensions")]
            let per_agent_codemode: Option<Arc<dyn AgentPlugin>> =
                if agent_dir_explicit && name == "codemode" {
                    Some(Arc::new(
                        crate::plugins::codemode::CodemodePlugin::with_config(codemode_config_for(
                            &agent_dir, &cwd,
                        )),
                    ))
                } else {
                    None
                };
            #[cfg(not(feature = "extensions"))]
            let per_agent_codemode: Option<Arc<dyn AgentPlugin>> = None;
            if let Some(plugin) = per_agent_codemode {
                user_plugins.push(plugin);
                continue;
            }
            let plugin = crate::plugins::instantiate_builtin(name).ok_or_else(|| {
                SdkError::Other(crate::plugins::builtin::missing_builtin_message(name))
            })?;
            user_plugins.push(rewire_builtin_for_session(plugin, &agent_dir, &cwd));
        }
        user_plugins.extend(self.plugins);
        let sink_slot = Arc::new(std::sync::Mutex::new(None));
        user_plugins.push(Arc::new(super::event::SdkBridgePlugin::new(Arc::clone(
            &sink_slot,
        ))));
        // Build the unified plugin host BEFORE the session so the tool
        // selection is resolved against the REAL merged registry (base tools
        // plus every builtin/plugin/JS tool). The exact host is handed to the
        // session below, so factories and their registration pass run once.
        // The host's settings-driven builtin gates are resolved from the
        // agent's OWN directory (D2); the default path keeps the shared
        // environment-resolving host byte-for-byte.
        let plugin_host = if agent_dir_explicit {
            crate::agent_session::AgentSession::build_plugin_host_in(
                extensions.as_ref(),
                user_plugins.clone(),
                Some(cwd.as_path()),
                Some(agent_dir.as_path()),
            )
        } else {
            crate::agent_session::AgentSession::build_plugin_host(
                extensions.as_ref(),
                user_plugins.clone(),
                Some(cwd.as_path()),
            )
        };
        // Strict attachment contract (the same one `extension_paths` and
        // `builtin(...)` use): a plugin that was explicitly handed to the
        // builder and whose `register()` failed is NOT silently absent. The
        // host isolates the failure so unrelated plugins keep working; the
        // SDK build surfaces it instead of reporting success.
        let registration_failures = plugin_host.registration_failures();
        if !registration_failures.is_empty() {
            let message = registration_failures
                .iter()
                .map(|failure| {
                    format!(
                        "plugin `{}` failed to register: {}",
                        failure.path, failure.message
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SdkError::Other(message));
        }

        // --- tool selection ----------------------------------------------
        // Resolved through the same shared validation the CLI flags use, so
        // names, `*` patterns, and the `+name`/`-name` modifier forms all
        // behave identically. The executable registry stays unfiltered (F12):
        // the resolved selection is the session's ACTIVE override, not a
        // registry cap, so an allowlisted codemode/MCP tool is still
        // reachable and late plugin registrations are not pruned. When no
        // selection method was called, `None` leaves the exposure defaults
        // untouched.
        //
        // Two sets are needed:
        // - `available_tools`: every non-hidden tool. A plain allowlist may
        //   name a codemode/deferred tool and thereby activate it explicitly.
        // - `defaults`: only tools that activate on registration (`direct`
        //   and active `model-only` tools). Codemode/deferred/inactive
        //   model-only tools stay callable but undeclared unless named.
        let mut available_tools: Vec<String> = tools
            .iter()
            .filter(|tool| tool.exposure() != crate::agent::tool_execution::ToolExposure::Hidden)
            .map(|tool| tool.name().to_string())
            .collect();
        for (definition, _) in plugin_host.registry().resolved_tools() {
            if definition.exposure != "hidden"
                && !available_tools.iter().any(|name| name == &definition.name)
            {
                available_tools.push(definition.name.clone());
            }
        }
        // Deterministic matching order: `tools` is a HashMap, and glob
        // allowlists emit names in `available_tools` order.
        available_tools.sort();
        available_tools.dedup();
        let mut defaults: Vec<String> = tools
            .iter()
            .filter(|tool| {
                tool.exposure()
                    .active_on_registration(tool.default_active())
            })
            .map(|tool| tool.name().to_string())
            .collect();
        for (definition, _) in plugin_host.registry().resolved_tools() {
            let exposure = crate::agent::tool_execution::ToolExposure::parse(&definition.exposure)
                .unwrap_or(crate::agent::tool_execution::ToolExposure::Direct);
            if exposure.active_on_registration(definition.default_active)
                && !defaults.iter().any(|name| name == &definition.name)
            {
                defaults.push(definition.name.clone());
            }
        }
        // Deterministic declaration order: the base registry is a HashMap.
        defaults.sort();
        defaults.dedup();
        let default_names: Vec<&str> = defaults.iter().map(String::as_str).collect();

        // An explicitly empty `.tools([])` is an explicit empty allowlist,
        // not the absence of a selection. `.exclude_tools([])` never makes it
        // here as a request: the setter leaves `tool_selection_requested`
        // false and only a non-empty denylist counts below.
        let explicit_empty_allowlist =
            self.tool_allowlist_requested && self.tool_allowlist.is_empty();
        let selection_requested = self.tool_selection_requested || !self.tool_denylist.is_empty();
        let mut tool_selection = if explicit_empty_allowlist {
            Some(crate::coding_agent::ToolSelection {
                enabled: Vec::new(),
                modifiers: Vec::new(),
                uses_default_tools: false,
            })
        } else if self.no_tools {
            // `noTools` suppresses the DEFAULT set only (upstream sdk.ts:282-294):
            // an explicit `.tools(...)` selection still becomes the active set,
            // and a pure modifier list applies to the empty default base.
            if self.tool_allowlist.is_empty() {
                Some(crate::coding_agent::ToolSelection::default())
            } else {
                let default_names: Vec<&str> = defaults.iter().map(String::as_str).collect();
                let modifier_only = self
                    .tool_allowlist
                    .iter()
                    .all(|entry| crate::config::settings::is_tool_modifier(entry));
                let resolve_available: &[String] = if modifier_only {
                    &defaults
                } else {
                    &available_tools
                };
                let empty_defaults: Vec<String> = Vec::new();
                let mut selection = crate::coding_agent::tool_selection::resolve_tool_selection(
                    Some(empty_defaults.as_slice()),
                    &self.tool_allowlist,
                    &self.tool_denylist,
                    false,
                    false,
                    resolve_available,
                    &default_names,
                )
                .map_err(SdkError::Other)?;
                if let Some(selection) = selection.as_mut() {
                    // The resolver starts from an empty default base here; keep
                    // exactly the explicit entries/patterns. (The resolver no
                    // longer appends MCP carve-out names — `enabled` is the
                    // declared set — so this is a guard, not a correction.)
                    selection.enabled = retain_explicitly_selected(
                        &self.tool_allowlist,
                        std::mem::take(&mut selection.enabled),
                    );
                    selection.uses_default_tools = false;
                    selection.modifiers.clear();
                }
                selection
            }
        } else if selection_requested {
            // Modifier/exclude-only forms start from the active-on-registration
            // set: the base is that set, not the full registry, so an unnamed
            // tool is never activated by a denylist (the resolver no longer
            // carves MCP names into `enabled`). A plain allowlist keeps the
            // full non-hidden set so an explicit codemode/deferred/MCP name
            // (or glob) can activate it.
            let modifier_or_exclude_only = self.tool_allowlist.is_empty()
                || self
                    .tool_allowlist
                    .iter()
                    .all(|entry| crate::config::settings::is_tool_modifier(entry));
            let resolve_available: &[String] = if modifier_or_exclude_only {
                &defaults
            } else {
                &available_tools
            };
            crate::coding_agent::tool_selection::resolve_tool_selection(
                // The settings `defaultTools` list is the CLI's base for the
                // modifier/exclude-only forms (upstream `sdk.ts:282-286`); an
                // explicit `.tools(...)`/`.no_tools()` still replaces it.
                settings_default_tools
                    .as_deref()
                    .or(Some(defaults.as_slice())),
                &self.tool_allowlist,
                &self.tool_denylist,
                false,
                false,
                resolve_available,
                &default_names,
            )
            .map_err(SdkError::Other)?
        } else if let Some(settings_tools) = settings_default_tools.as_deref() {
            // No builder selection was requested: the settings `defaultTools`
            // list is the active set (upstream `initialActiveToolNames`), with
            // the same validation the CLI applies (`_isActivatable`: an MCP
            // name the list does not name stays registered, not declared). A
            // `+name`/`-name`-only list was already resolved against
            // `DEFAULT_TOOL_NAMES` by `resolve_default_tools`.
            crate::coding_agent::tool_selection::resolve_tool_selection(
                Some(settings_tools),
                &[],
                &[],
                false,
                false,
                &available_tools,
                &default_names,
            )
            .map_err(SdkError::Other)?
        } else {
            None
        };
        // `resolve_tool_selection` owns the explicit/default classification:
        // plain allowlists and `no_tools` are explicit, while modifier and
        // exclude-only forms stay default-derived so a resumed session can
        // restore the loadout recorded in its transcript. Do not overwrite it.
        //
        // A restored loadout replaces `enabled` with the transcript's names,
        // so re-express the current exclusions as `-name` modifiers: they are
        // re-applied after the recorded loadout on resume. The expansion uses
        // the same name matcher as the shared resolver (only `*` is a
        // wildcard, `?` is literal, matching is case-sensitive), so a resumed
        // session excludes exactly the names a fresh build excludes:
        // `.exclude_tools(["READ"])`/`["re?d"]` must not turn into `-read`.
        if let Some(selection) = tool_selection.as_mut() {
            if selection.uses_default_tools && !self.tool_denylist.is_empty() {
                let denied: Vec<String> = available_tools
                    .iter()
                    .filter(|name| {
                        self.tool_denylist
                            .iter()
                            .any(|pattern| tool_name_pattern_matches(pattern, name.as_str()))
                    })
                    .cloned()
                    .collect();
                selection
                    .modifiers
                    .extend(denied.into_iter().map(|name| format!("-{name}")));
            }
        }

        // --- thinking level ----------------------------------------------
        // Builder override wins; an existing session's recorded level is
        // restored next (unless the builder pinned one); environment
        // resolution may have supplied a per-model/settings level; otherwise
        // the upstream default (medium).
        let mut thinking = self.thinking_level.unwrap_or_else(|| {
            resolved_thinking.unwrap_or(crate::ai::providers::resolution::DEFAULT_THINKING_LEVEL)
        });
        if !thinking_pinned {
            if let Some(level) = session
                .as_ref()
                .and_then(super::agent::session_recorded_thinking)
            {
                thinking = level;
            }
        }

        // --- system prompt ------------------------------------------------
        // Only an explicit `.system_prompt(..)` replaces the derived prompt
        // (upstream `customPrompt`, system-prompt.ts:152-170). An appendix on
        // its own is the `addendum` of the DEFAULT prompt
        // (system-prompt.ts:172: `if (appendSystemPrompt) promptSections.addendum`),
        // so it must not become the whole prompt: the session would treat it
        // as an explicit override, skip the base `<tools>`/`<rules>`/`<cwd>`
        // sections, and stop following active-set/`/reload` rebuilds. It is
        // handed to the session AFTER construction (still before the first
        // prompt) so the derived prompt keeps its cache and the addendum is
        // re-applied on every rebuild.
        let append_to_derived = self.system_prompt.is_none() && self.append_system_prompt.is_some();
        let append_system_prompt = self.append_system_prompt;
        let system_prompt = match (self.system_prompt, append_system_prompt.as_deref()) {
            (Some(prompt), Some(appendix)) => Some(format!("{prompt}\n\n{appendix}")),
            (Some(prompt), None) => Some(prompt),
            (None, _) => None,
        };

        // --- assemble the session -----------------------------------------
        // The directory `Agent::new_session` creates fresh session files in
        // (the same directory the build's own storage uses). An explicit
        // `Existing` file reuses its parent; a pathless file falls back to
        // the default session directory.
        //
        // D9: this root is build-bound. It is resolved once here, from the
        // agent's OWN directory (`.agent_dir(..)` when supplied, else the
        // environment's), and `Agent::new_session`/`continue_session` never
        // re-resolve it — a session belongs to the directory it was created
        // under. `Agent::agent_dir`/`Agent::session_dir` report the binding;
        // building a new agent is how a root changes.
        let session_dir = match &self.session {
            SessionMode::None => None,
            SessionMode::Dir(dir) => Some(dir.clone()),
            SessionMode::Existing(path) => Some(
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| default_session_dir_in(&agent_dir, &cwd_text)),
            ),
            SessionMode::DefaultDir => Some(default_session_dir_in(&agent_dir, &cwd_text)),
        };
        let session_cwd = cwd.clone();
        // An environment-resolved key is provider-specific; the SDK must not
        // later send it to a different provider restored from a session.
        let api_key_provider = if !from_endpoint && self.api_key.is_none() && api_key.is_some() {
            Some(model.provider.clone())
        } else {
            None
        };
        // `new_arc_with_plugins_and_host_in` binds the phone builtin's live
        // adapter to this session `Arc` (the adapter holds a `Weak`, so no
        // cycle), reuses the host the selection was resolved against, and
        // binds the session's agent-directory identity (D1/D2) so every
        // session-scoped settings/auth/sessions-root read of this agent comes
        // from `agent_dir`.
        let tool_hooks = self.tool_hooks;
        let plugin_host_for_init = Arc::clone(&plugin_host);
        let session_facade = crate::agent_session::AgentSession::new_arc_with_plugins_and_host_in(
            crate::agent_session::AgentSessionConfig {
                provider_registry: Arc::clone(&registry),
                model,
                api_key: api_key.clone(),
                thinking,
                tools,
                session,
                compaction: self.compaction,
                // The SDK's default is the non-interactive guard
                // (`DEFAULT_AGENT_MAX_TURNS`, 1000); an explicit
                // `.max_turns(..)` always wins. `PIR_MAX_TURNS` is a host
                // process knob, not part of the hermetic builder contract.
                max_turns: self
                    .max_turns
                    .unwrap_or(crate::config::defaults::DEFAULT_AGENT_MAX_TURNS),
                system_prompt,
                cwd: Some(cwd),
                event_stream: None,
                extensions,
                tool_selection,
            },
            user_plugins,
            Some(plugin_host),
            Some(agent_dir.clone()),
        );
        // Clamp the starting thinking level to what the resolved model
        // supports (upstream `createAgentSession`'s
        // `clampThinkingLevel(model, thinkingLevel)`): a non-reasoning model
        // reports `off`, and a level the model's map does not name is clamped
        // down instead of being sent to the provider. `Agent::thinking_level`
        // promises the EFFECTIVE level, so the clamp must land before the
        // first run.
        session_facade.set_thinking_level(thinking).await;
        // Per-tool lifecycle hooks are installed before the first run; a
        // later `Agent::set_tool_hooks` replaces them.
        if tool_hooks.is_some() {
            session_facade.set_tool_hooks(tool_hooks);
        }
        // Append-only form: extend the just-derived base prompt and record the
        // addendum on its cache, so active-set rebuilds and `/reload` keep it
        // as the last section (upstream re-renders the `addendum` section from
        // the resource loader's append list on every `_rebuildSystemPrompt`).
        if append_to_derived {
            if let Some(appendix) = append_system_prompt {
                session_facade.append_derived_system_prompt(appendix);
            }
        }
        // Resource loader: the same install step `create_agent_session`
        // performs. Reload once (discovery is synchronous and bounded) and
        // hand the result to the session before the first prompt, so skills
        // and prompt templates are live for the derived system prompt and
        // `/skill:` expansion. No loader = no implicit discovery (hermetic),
        // matching the lower-level factories.
        if let Some(mut loader) = self.resource_loader {
            let resources = loader.reload();
            if !resources.skills.is_empty() || !resources.prompt_templates.is_empty() {
                session_facade
                    .set_prompt_resources_shared(resources.skills, resources.prompt_templates);
            }
        }
        let agent = Agent::new(
            session_facade,
            // The bridge plugin and the Agent share ONE sink slot: live
            // `tool_execution_update` payloads flow into the streaming
            // run's event stream.
            sink_slot,
            // Paths the SDK lifecycle methods need after the build.
            session_cwd,
            session_dir,
            agent_dir,
            model_pinned,
            thinking_pinned,
            initial_usage,
            api_key_provider,
        );
        if self.max_retries.is_some() || self.max_retry_delay_ms.is_some() {
            agent
                .inner
                .session
                .set_retry(self.max_retries, self.max_retry_delay_ms);
        }
        // Plugin lifecycle: the host's service context was bound by the
        // session constructor above and no event has been dispatched yet, so
        // this is the documented `on_init` position (after `register` and
        // `on_context`, before the first event). Failures are contained (the
        // host logs and reports each one); they never fail an otherwise
        // usable build.
        let init_failures = plugin_host_for_init.run_init_hooks().await;
        for (path, error) in &init_failures {
            crate::ext_diag!("[sdk] plugin `{path}` init failed: {error}");
        }
        Ok(agent)
    }
}

/// Keep only the names an explicit `.tools(...)` list selected. Used for the
/// `no_tools` + explicit-list path: the shared resolver starts from an empty
/// default base there, and this pins the selection to exactly the explicit
/// entries/patterns even if the resolver's base handling changes.
///
/// Exposed for the `tests/suites/sdk_units.rs` integration suite; not part of
/// the stable SDK surface.
#[doc(hidden)]
pub fn retain_explicitly_selected(allowlist: &[String], enabled: Vec<String>) -> Vec<String> {
    let modifier_only = allowlist
        .iter()
        .all(|entry| crate::config::settings::is_tool_modifier(entry));
    if modifier_only {
        let empty: Vec<String> = Vec::new();
        let selected = crate::config::settings::apply_tool_modifiers(&empty, allowlist);
        enabled
            .into_iter()
            .filter(|name| selected.iter().any(|selected| selected == name))
            .collect()
    } else {
        enabled
            .into_iter()
            .filter(|name| {
                // Same matcher as the shared resolver's `match_allowlist`
                // (upstream `createToolNameMatcher`): only `*` is a wildcard,
                // `?` is a literal name character, and matching is
                // case-sensitive.
                allowlist
                    .iter()
                    .any(|entry| tool_name_pattern_matches(entry, name))
            })
            .collect()
    }
}

/// Upstream `toolPatternRegExp` semantics (mcp-servers.ts:12-24): `*` matches
/// any run of characters (including none), every other character — `?`
/// included — is literal, and matching is case-sensitive.
///
/// This mirrors the shared resolver's private `tool_pattern_matches`
/// (`crate::coding_agent::tool_selection`); the SDK builder must not use
/// [`crate::coding_agent::tool_selection::glob_matches`] for tool names,
/// whose `?` wildcard and case folding are MODEL-SCOPE semantics. Using it
/// here made `.exclude_tools(["READ"])`/`["re?d"]` re-express as `-read`, so
/// a fresh build kept `read` while a resumed build (whose recorded loadout is
/// the modifier base) dropped it.
fn tool_name_pattern_matches(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[u8], text: &[u8]) -> bool {
        match (pattern.first(), text.first()) {
            (None, None) => true,
            (Some(b'*'), _) => (0..=text.len()).any(|skip| matches(&pattern[1..], &text[skip..])),
            (Some(expected), Some(actual)) if expected == actual => {
                matches(&pattern[1..], &text[1..])
            }
            _ => false,
        }
    }
    matches(pattern.as_bytes(), text.as_bytes())
}

/// Session-scoped re-rooting for a builtin instantiated from
/// [`AgentBuilder::builtin`], mirroring `PluginHost::build`'s resolution of
/// `builtin:<name>` path-plan entries: the `mcp` builtin re-roots at the
/// session cwd so `.pi/mcp.json` discovery and stdio spawns follow it
/// (upstream per-session cwd), not the process cwd the factory default uses.
/// The `subagent` builtin re-roots at the agent's own `agents` directory, so
/// an explicit [`AgentBuilder::agent_dir`] is honored for user-agent
/// discovery and for the tool description (the factory default reads the
/// process environment).
///
/// D2: the manager's OTHER root — `mcp.log`, `mcp-auth.json`, and the global
/// `mcp.json` — is the agent's own directory (`agent_dir`), captured once
/// here. Before this the builtin re-read the process environment, so two SDK
/// agents built around different directories in one process shared the first
/// one's log/credential store.
fn rewire_builtin_for_session(
    plugin: Arc<dyn AgentPlugin>,
    agent_dir: &Path,
    cwd: &std::path::Path,
) -> Arc<dyn AgentPlugin> {
    #[cfg(feature = "mcp")]
    if plugin
        .as_any()
        .and_then(|any| any.downcast_ref::<crate::plugins::builtin::McpPlugin>())
        .is_some()
    {
        return Arc::new(crate::plugins::builtin::McpPlugin::with_cwd(agent_dir, cwd));
    }
    #[cfg(feature = "subagent")]
    if plugin
        .as_any()
        .and_then(|any| any.downcast_ref::<crate::plugins::SubagentPlugin>())
        .is_some()
    {
        return Arc::new(crate::plugins::SubagentPlugin::with_user_agents_dir(
            agent_dir.join("agents"),
        ));
    }
    #[cfg(not(feature = "mcp"))]
    let _ = cwd;
    #[cfg(not(any(feature = "mcp", feature = "subagent")))]
    let _ = agent_dir;
    plugin
}

/// The default session directory for `cwd` under an EXPLICIT agent directory
/// (`<agent_dir>/sessions/<encoded-cwd>`; the `.agent_dir(..)` form of
/// [`crate::session_manager::default_session_dir_path`], which resolves the
/// environment itself).
fn default_session_dir_in(agent_dir: &Path, cwd: &str) -> PathBuf {
    crate::session_manager::sessions_dir_in(agent_dir).join(
        crate::session_manager::encode_cwd_dir_name(&crate::session_manager::resolve_path_str(cwd)),
    )
}

/// `has_configured_auth` with the stored-credential read rooted at an explicit
/// agent directory (D2): the shared version consults
/// [`crate::ai::auth::FileAuthStorage::default_storage`], which re-resolves
/// the process environment, so an SDK agent with its own directory would see
/// another directory's `auth.json`.
pub(crate) fn has_configured_auth_in(
    agent_dir: &Path,
    provider_id: &str,
    api_keys: &std::collections::BTreeMap<String, String>,
) -> bool {
    if api_keys.contains_key(provider_id) {
        return true;
    }
    if crate::ai::auth::env_api_keys::all()
        .into_iter()
        .any(|(id, keys)| id == provider_id && keys.iter().any(|key| std::env::var(key).is_ok()))
    {
        return true;
    }
    crate::ai::auth::FileAuthStorage::in_agent_dir(agent_dir)
        .read_stored(provider_id)
        .map(|stored| stored.is_some())
        .unwrap_or(false)
}

/// Load the pi environment (`settings.json` + `models.json` + per-provider
/// keys) from an EXPLICIT agent directory (D2).
///
/// The shared [`crate::ai::providers::load_pi_environment`] resolves the
/// global scope from the process environment and takes no directory, so this
/// composes the same public primitives in the same order: the embedded
/// catalog first, the `models.json` providers upserted on top through
/// [`crate::ai::providers::merge_pi_config_in`] (per-provider composition,
/// `modelOverrides`, `authHeader`, `!command`/`$ENV` headers), and the same
/// per-provider API-key template resolution. Both the keys and the headers
/// are keyed by THIS agent dir's config stamps.
///
/// Documented difference from the shared loader: the Claude Code environment
/// compatibility layer (`ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` bootstrapping)
/// is not re-applied. Those variables are process-global by nature, and an
/// explicit agent directory names configuration FILES, not an environment.
pub(crate) fn load_environment_at(
    agent_dir: &Path,
    cwd: &Path,
) -> Result<crate::ai::providers::PiEnvironment, SdkError> {
    let project_trusted = crate::config::trust::resolve_cwd_trust(cwd, agent_dir);
    let manager = crate::config::settings::SettingsManager::load(
        agent_dir,
        &cwd.join(".pi"),
        cwd,
        project_trusted,
    );
    let errors = manager.error_messages();
    if !errors.is_empty() {
        return Err(SdkError::Other(errors.join("; ")));
    }
    let settings_snapshot = manager.settings.clone();

    let config = match std::fs::read_to_string(agent_dir.join("models.json")) {
        Ok(contents) => crate::ai::providers::parse_pi_config(&contents)
            .map_err(|error| SdkError::Other(format!("invalid pi models.json: {error}")))?,
        Err(_) => crate::ai::providers::PiModelsConfig::default(),
    };

    // Per-provider resolved API keys (models.json templates). The `!command`
    // form uses the (agent dir, command) cache through
    // `resolve_config_value_in`, so this agent's command results are cached
    // under ITS directory and never served to another one (C2).
    let mut api_keys = std::collections::BTreeMap::new();
    for (provider_id, provider_config) in &config.providers {
        if let Some(key) = provider_config.api_key.as_deref().and_then(|key| {
            crate::ai::providers::config_value::resolve_config_value_in(agent_dir, key, None)
        }) {
            api_keys.insert(provider_id.clone(), key);
        }
    }

    // Registry: embedded catalog first, then the composed models.json
    // providers on top (upsert order keeps models.json entries winning per
    // provider), exactly like the shared loader.
    let mut registry = crate::ai::provider::ProviderRegistry::default();
    crate::ai::providers::register_builtin_catalog(&mut registry)?;
    let mut merged: std::collections::BTreeMap<String, Vec<Model>> =
        std::collections::BTreeMap::new();
    for model in crate::ai::providers::merge_pi_config_in(agent_dir, &config)? {
        merged
            .entry(model.provider.clone())
            .or_default()
            .push(model);
    }
    for (provider_id, provider_config) in &config.providers {
        let models = merged.entry(provider_id.clone()).or_default();
        if models.is_empty() {
            // Legacy single-model provider entry (e.g. a local Ollama
            // endpoint) with no model list and no catalog entry: synthesize
            // it, like the shared loader.
            let model_id = provider_config
                .model
                .clone()
                .or_else(|| provider_config.models.first().map(|entry| entry.id.clone()))
                .unwrap_or_else(|| "default".to_string());
            models.push(crate::ai::providers::construction::model(
                provider_id,
                &model_id,
                &model_id,
                provider_config.api.as_deref().unwrap_or("openai-chat"),
                provider_config
                    .base_url
                    .as_deref()
                    .unwrap_or("http://localhost:11434/v1"),
                128_000,
                16_384,
                vec!["text"],
            ));
        }
        let display_name = provider_config
            .name
            .clone()
            .or_else(|| {
                crate::ai::providers::catalog::load_provider(provider_id)
                    .ok()
                    .flatten()
                    .map(|builtin| builtin.display_name)
            })
            .unwrap_or_else(|| provider_id.clone());
        registry.set_provider(crate::ai::providers::CatalogProviderEntry::from_models(
            provider_id,
            &display_name,
            models,
        )?);
    }

    let models = registry.get_models();
    Ok(crate::ai::providers::PiEnvironment {
        registry,
        models,
        api_keys,
        settings: settings_snapshot,
    })
}

/// The `codemode` builtin's config resolved from an EXPLICIT agent
/// directory (D2/C3): the per-agent form of
/// [`crate::plugins::codemode::load_codemode_config`], which resolves the
/// global settings scope from the process environment.
///
/// The `PIR_CODEMODE_MEMORY_MB` override stays process-global (an embedder
/// seam, documented as such by the builtin) and is applied last, exactly
/// like the shared loader.
#[cfg(feature = "extensions")]
fn codemode_config_for(agent_dir: &Path, cwd: &Path) -> crate::plugins::codemode::CodemodeConfig {
    let trusted = std::env::var("PIR_TRUST_PROJECT").is_ok_and(|value| value == "1");
    let mut manager =
        crate::config::settings::SettingsManager::load(agent_dir, &cwd.join(".pi"), cwd, trusted);
    for error in manager.drain_errors() {
        crate::ext_diag!(
            "[codemode] settings {}: {}",
            error.scope_name(),
            error.message
        );
    }
    let mut config = crate::plugins::codemode::codemode_config_from(&manager);
    if let Some(override_bytes) = sandbox_memory_env_override() {
        config.sandbox_memory_bytes = override_bytes;
    }
    config
}

/// The `PIR_CODEMODE_MEMORY_MB` override, same contract as the builtin's own
/// resolver (`Some` for a valid non-negative integer, `0` = no cap, `None`
/// when absent or invalid — an invalid value is reported once and ignored).
///
/// Duplicated here because the builtin's resolver is private to
/// `plugins::codemode`, and the SDK only needs it on the explicit-agent-dir
/// path (the environment path calls the builtin's own loader).
#[cfg(feature = "extensions")]
fn sandbox_memory_env_override() -> Option<Option<usize>> {
    let raw = std::env::var("PIR_CODEMODE_MEMORY_MB").ok()?;
    match raw.trim().parse::<u64>() {
        Ok(0) => Some(None),
        Ok(mb) => Some(Some(
            usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX),
        )),
        Err(_) => {
            crate::ext_diag!(
                "pir-codemode: PIR_CODEMODE_MEMORY_MB=\"{raw}\" ignored: expected MiB as a non-negative integer"
            );
            None
        }
    }
}

/// [`crate::ai::providers::install_remote_catalog_overlays`] rooted at an
/// EXPLICIT agent directory (D2/C12): the overlays persist their
/// `models-store.json` ETag/model cache under `agent_dir` instead of the
/// process environment's directory. Same gating (`PI_OFFLINE`,
/// `PI_CATALOG_BASE_URL`) and the same per-provider failure isolation.
fn install_remote_catalog_overlays_at(
    registry: &Arc<crate::ai::provider::ProviderRegistry>,
    agent_dir: &Path,
) -> usize {
    if crate::config::env::is_offline() {
        return 0;
    }
    let base = std::env::var("PI_CATALOG_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::ai::providers::REMOTE_CATALOG_DEFAULT_BASE_URL.to_string());
    let mut spawned = 0usize;
    for (provider_id, _) in crate::ai::providers::catalog::CATALOG_FILES {
        let config = crate::ai::providers::RemoteCatalogOverlayConfig {
            provider_id: (*provider_id).to_string(),
            catalog_url: format!(
                "{base}/api/models/providers/{provider_id}?types=chat,image,classifier"
            ),
            agent_dir: agent_dir.to_path_buf(),
            interval: None,
        };
        let overlay =
            match crate::ai::providers::RemoteCatalogOverlay::new_with_provider_personalizer(
                config,
                Arc::clone(registry),
            ) {
                Ok(overlay) => Arc::new(overlay),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider_id,
                        %error,
                        "remote catalog overlay install failed; skipping provider"
                    );
                    continue;
                }
            };
        if overlay.spawn_refresh_task() {
            spawned += 1;
        }
    }
    spawned
}
