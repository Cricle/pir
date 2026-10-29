//! The shared host-service layer: typed traits + the [`PluginServices`]
//! builder.
//!
//! This module is the Rust face of the service spine described in
//! `logs/perf/agent-parity-audit.md` (§2). One implementation of a service
//! backs both faces: the Rust [`PluginContext`](super::context::PluginContext)
//! calls these methods with typed values; the QuickJS host functions become
//! thin `parse JSON -> call service -> serialize reply` adapters.
//!
//! Rules this contract enforces:
//!
//! - **No JSON re-serialization for the Rust path.** Where a typed value
//!   exists (`Model`, `ThinkingLevel`, `ToolDef`, `CommandDef`, ...) the
//!   signature uses it. Where the underlying data is inherently JSON (session
//!   tree rows, tool args/results, provider/virtual-model definitions,
//!   settings) the signature takes or returns `serde_json::Value` directly —
//!   no `to_value`/`from_str` on the Rust path.
//! - **Cheap reads.** Synchronous reads are served by the implementation from
//!   its cached state and return `Arc`/borrowed data. The context accessors
//!   themselves are pure field reads; the only cost of a call is the `Arc`
//!   clone the service returns.
//! - **Async only where work is external.** Dialogs, `execute_tool`,
//!   `exec`, `complete`, `wait_for_idle`, and completion suggestion are async
//!   because the JS face also awaits them. Session/message actions stay
//!   synchronous and enqueue into the exact same action queue the JS host
//!   functions use (the parity limit documented in audit §2.3).
//! - **Optional capabilities fail closed.** Every service is optional on the
//!   context; accessors return `None` and the default trait methods return
//!   [`PluginServiceError::Unavailable`] instead of fabricating a result.
//!
//! ## Epoch / `/reload` rule
//!
//! A [`PluginContext`] carries the host generation it was bound at
//! ([`PluginContext::epoch`]) plus the live generation cell owned by the
//! host. [`PluginHost::bind_context`](crate::plugins::PluginHost::bind_context)
//! increments the cell and stamps the new context with the new value. Any
//! context bound earlier — including clones a plugin cached — therefore
//! reports [`PluginContext::is_active`] `false` and
//! [`PluginContext::check_active`] `Err(Stale)`. Rebuilding the session,
//! `/reload`, `newSession`, `fork`, and `switchSession` must all rebind a
//! fresh context; that single call is the invalidation point. See
//! [`PluginContext`] for the exact contract.
//!
//! ## Not a second event dispatch table
//!
//! [`EventService`] is the `pi.events` extension *bus* (Rust ↔ Rust
//! subscriptions), not the unified plugin event dispatch. Event handlers stay
//! on [`AgentPlugin::events`](crate::plugins::plugin::AgentPlugin::events) /
//! [`AgentPlugin::on_event`](crate::plugins::plugin::AgentPlugin::on_event) —
//! one ordered dispatch, one merge layer. Do not add a parallel handler list.
//!
//! ## Provider auth seam
//!
//! [`ProviderAuthHooks`] is the typed native replacement for the last JS-only
//! provider capability (`pi.registerProvider`'s `oauth.login` /
//! `refreshToken` / `getApiKey` callbacks, audit §1.9 and §5.3.5). A plugin
//! registers it through [`ProviderService::register_auth_hooks`] and the
//! login/refresh/logout entry points consult
//! [`native_provider_auth_hooks`] before the builtin OAuth flow or a
//! JS-backed flow:
//!
//! - **Precedence:** a native hook for provider X wins over a JS callback for
//!   X (including `ai::providers::js_stream::JsOAuthFlow`). Exactly one flow
//!   runs: the native hook, or — only when it answers
//!   [`ProviderAuthError::Unavailable`] — the one fallback flow.
//! - **Persistence:** the same `~/.pi/agent/auth.json` store and
//!   read-modify-write path as builtins (`StoredCredential::ApiKey` for
//!   [`ProviderAuth::api_key`], `StoredCredential::OAuth` for
//!   [`ProviderAuth::access_token`]/[`ProviderAuth::refresh_token`]).
//!   `Failed`/`Cancelled` are returned before any store write, so the old
//!   credential stays intact.
//! - **Lifetime:** the table is process-wide but stores no session/host
//!   reference; the live service unregisters only the entries it still owns
//!   when its context is dropped, and application code can call
//!   [`ProviderService::unregister_auth_hooks`] explicitly.
//!
//! Unsupported flows (exact reasons):
//!
//! - **Custom browser/component UI** (`ctx.ui.custom`-style): there is no
//!   shared component ABI a hook may rely on; the seam exposes only the
//!   host's [`AuthInteraction`] prompts and [`crate::ai::auth::AuthEvent`]
//!   notifications. A hook needing more returns
//!   [`ProviderAuthError::Unavailable`]. (An extension's own
//!   `ctx.ui.custom` overlay IS supported — see [`UiService::custom`]; this
//!   bullet is about provider-auth hooks, which have no UI thread of their
//!   own.)
//! - **Host-owned device-code polling:** the hook may emit
//!   `AuthEvent::DeviceCode` and prompt with `AuthPrompt::ManualCode`, but no
//!   host service owns the device-code polling loop; the hook must perform
//!   its own polling.
//! - **TUI `/login` provider enumeration:** the selector is built from the
//!   static `OAUTH_PROVIDERS` list, so a hook-only provider is reachable via
//!   `pir auth login <id>` but is not listed in the TUI selector (that list is
//!   outside this module's file scope).
//! - **Plugin hooks in the standalone `pir auth login` process:** that
//!   command runs before any extension/plugin host is built, so only hooks
//!   registered in-process by builtins, embedders, or tests are visible.
//!   Hooks registered by session plugins are used by the live `/login`
//!   (TUI/RPC) path and by token refresh inside a running session.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value as JsonValue;

// Re-exported typed service data. These stay aliases (not new wrappers) so
// the plugin registry and the service layer can never disagree about a
// definition shape.
pub use crate::agent_session::ContextUsage;
pub use crate::ai::providers::resolution::ScopedModel;
pub use crate::extensions::{CommandDef as CommandInfo, ToolDef as ToolInfo};

use crate::agent::nested_tool_calls::NestedToolOutcome;
use crate::ai::auth::{AuthInteraction, LoginOptions, OAuthCredential, StoredCredential};
use crate::ai::provider::ProviderBox;
use crate::ai::types::{
    AbortSignal, AssistantMessage, Context as ModelContext, Model, ThinkingLevel,
};
use crate::extensions::{FlagValue, RendererCallOutcome, RendererOutcome, SessionContextSnapshot};

use super::context::{PluginContext, PluginIdentity};

// ---------------------------------------------------------------------------
// Futures / errors
// ---------------------------------------------------------------------------

/// Boxed async return used by the service traits.
///
/// The service layer is a trait-object boundary, so every async method boxes
/// exactly like the upstream JS APIs those methods replace.
pub type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Typed service failure. No stringly-typed errors cross the plugin boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginServiceError {
    /// The service is not bound on this host.
    Unavailable {
        service: &'static str,
        capability: &'static str,
    },
    /// The context was bound at `epoch`, but the host has moved on. The
    /// caller is holding a stale [`PluginContext`] from before `/reload` or a
    /// session replacement.
    Stale { epoch: u64, current: u64 },
    /// The capability ran and failed.
    Failed(String),
}

impl PluginServiceError {
    /// The service is absent on this host.
    pub fn unavailable(service: &'static str) -> Self {
        Self::Unavailable {
            service,
            capability: "service",
        }
    }

    /// A specific capability of an otherwise present service is unavailable.
    pub fn capability(service: &'static str, capability: &'static str) -> Self {
        Self::Unavailable {
            service,
            capability,
        }
    }

    /// The context is stale: bound at `epoch`, host generation `current`.
    pub fn stale(epoch: u64, current: u64) -> Self {
        Self::Stale { epoch, current }
    }

    /// The capability failed.
    pub fn failed(error: impl Into<String>) -> Self {
        Self::Failed(error.into())
    }
}

impl std::fmt::Display for PluginServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable {
                service,
                capability,
            } => write!(f, "{service}: capability `{capability}` is unavailable"),
            Self::Stale { epoch, current } => write!(
                f,
                "stale plugin context: bound at epoch {epoch}, host is at epoch {current}"
            ),
            Self::Failed(error) => f.write_str(error),
        }
    }
}

impl std::error::Error for PluginServiceError {}

/// Convenience alias for service results.
pub type ServiceResult<T> = Result<T, PluginServiceError>;

// ---------------------------------------------------------------------------
// Shared typed data
// ---------------------------------------------------------------------------

/// How the session host is driving the agent (upstream `ctx.mode`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExtensionMode {
    /// Interactive TUI.
    Interactive,
    /// RPC host.
    Rpc,
    /// One-shot print host.
    Print,
    /// Headless/SDK host (the default when no mode was supplied).
    #[default]
    Headless,
    /// A mode string this contract does not know yet.
    Other(Arc<str>),
}

impl ExtensionMode {
    /// Classify the `ExtensionConfig::mode` string.
    pub fn from_mode(mode: &str) -> Self {
        match mode {
            "interactive" | "tui" => Self::Interactive,
            "rpc" => Self::Rpc,
            "print" => Self::Print,
            "headless" => Self::Headless,
            other => Self::Other(Arc::from(other)),
        }
    }

    /// The wire mode string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Interactive => "interactive",
            Self::Rpc => "rpc",
            Self::Print => "print",
            Self::Headless => "headless",
            Self::Other(mode) => mode,
        }
    }
}

/// Level of a `ctx.ui.notify` notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyLevel {
    #[default]
    Info,
    Warning,
    Error,
}

impl NotifyLevel {
    /// The wire level string (`info` / `warning` / `error`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// How a queued message should be delivered (upstream `deliverAs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverAs {
    /// Hold for the next turn.
    NextTurn,
    /// Steer the run in flight.
    Steer,
    /// Follow up after the current turn.
    FollowUp,
}

impl DeliverAs {
    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NextTurn => "nextTurn",
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

/// Options of `pi.sendMessage` (upstream `SendMessageOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendMessageOptions {
    /// `Some(false)` suppresses steering while busy; `None` keeps the
    /// upstream "absent is not false" behavior.
    pub trigger_turn: Option<bool>,
    /// Deliberate delivery channel.
    pub deliver_as: Option<DeliverAs>,
}

/// Options of `pi.sendUserMessage` (upstream `SendUserMessageOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendUserMessageOptions {
    pub deliver_as: Option<DeliverAs>,
}

/// A custom message as `pi.sendMessage` sends it (upstream `CustomMessage`).
///
/// `content` stays JSON on purpose: a custom message carries a string or
/// upstream content-block array, and every face reuses that wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: JsonValue,
    pub display: bool,
    pub details: Option<JsonValue>,
}

impl CustomMessage {
    /// Build a custom message; `display` defaults to `false` (upstream).
    pub fn new(custom_type: impl Into<String>, content: JsonValue) -> Self {
        Self {
            custom_type: custom_type.into(),
            content,
            display: false,
            details: None,
        }
    }

    /// Render this message in the transcript.
    pub fn with_display(mut self, display: bool) -> Self {
        self.display = display;
        self
    }

    /// Attach extension-defined renderer details.
    pub fn with_details(mut self, details: JsonValue) -> Self {
        self.details = Some(details);
        self
    }
}

/// Content of `pi.sendUserMessage` (upstream `string | content blocks`).
#[derive(Debug, Clone, PartialEq)]
pub enum UserContent {
    /// Plain text.
    Text(String),
    /// Upstream content blocks (inherently JSON).
    Blocks(Vec<JsonValue>),
}

impl UserContent {
    /// Plain text content.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }
}

/// Options of `pi.exec` (upstream `ExecOptions`).
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Working directory; `None` means the extension/session cwd.
    pub cwd: Option<PathBuf>,
    /// Timeout in milliseconds.
    pub timeout_ms: Option<f64>,
    /// Cancellation handle; an already-aborted handle kills the child
    /// immediately.
    pub signal: Option<CancellationHandle>,
}

/// Result of `pi.exec` (upstream `ExecResult`; same shape as
/// `extensions/runtime/exec.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub killed: bool,
}

/// Options of one `ctx.executeTool` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecuteToolOptions {
    /// Hand the nested call to the session's detach sink.
    pub background: bool,
}

/// Reference to a running tool call (`ctx.toolUpdate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRef {
    pub tool_call_id: Arc<str>,
    pub tool_name: Option<Arc<str>>,
    pub args: Option<Arc<JsonValue>>,
    pub parent_call_id: Option<Arc<str>>,
}

impl ToolCallRef {
    /// A reference by call id only.
    pub fn new(tool_call_id: impl Into<Arc<str>>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: None,
            args: None,
            parent_call_id: None,
        }
    }

    /// Attach the producing tool's name.
    pub fn with_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.tool_name = Some(name.into());
        self
    }

    /// Attach the call arguments.
    pub fn with_args(mut self, args: JsonValue) -> Self {
        self.args = Some(Arc::new(args));
        self
    }

    /// Attach the parent call id for nested calls.
    pub fn with_parent(mut self, parent: impl Into<Arc<str>>) -> Self {
        self.parent_call_id = Some(parent.into());
        self
    }
}

/// One selection choice of `ctx.ui.select` (upstream `SelectOption`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
}

impl SelectOption {
    /// A labelled option with a distinct value.
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }

    /// An option whose label and value are the same text.
    pub fn text(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
        }
    }
}

impl From<&str> for SelectOption {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

impl From<String> for SelectOption {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

/// Options shared by every dialog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DialogOptions {
    /// Optional deadline; the TUI applies its own default when `None`.
    pub timeout: Option<Duration>,
}

impl DialogOptions {
    /// A dialog with an explicit deadline.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
        }
    }
}

/// Custom streaming indicator (upstream `setWorkingIndicator` options).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingIndicator {
    /// Spinner frames; `Some(vec![])` hides the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds.
    pub interval_ms: Option<u64>,
}

/// Where a widget renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WidgetPlacement {
    #[default]
    AboveEditor,
    BelowEditor,
    Overlay,
}

impl WidgetPlacement {
    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AboveEditor => "aboveEditor",
            Self::BelowEditor => "belowEditor",
            Self::Overlay => "overlay",
        }
    }
}

/// What a raw terminal-input handler decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInputAction {
    /// Pass the input through unchanged.
    Pass,
    /// Consume the input.
    Consume,
    /// Replace the input with this text.
    Replace(String),
}

/// One theme known to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: Arc<str>,
    pub path: Option<Arc<Path>>,
}

/// Snapshot of the active theme. `data` is the theme's JSON definition
/// (inherently JSON in both faces).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeSnapshot {
    pub name: Arc<str>,
    pub data: Arc<JsonValue>,
}

/// Outcome of `set_theme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSetResult {
    /// The theme was applied.
    Applied,
    /// No theme with that name exists.
    NotFound,
    /// This host has no theme registry.
    Unavailable,
}

/// One extension-registered MCP server definition (upstream
/// `getMcpServers` returns extension registrations only).
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredMcpServer {
    pub name: Arc<str>,
    /// The registration config (inherently JSON).
    pub config: Arc<JsonValue>,
    /// The extension path that registered it, when known.
    pub extension_path: Option<Arc<Path>>,
}

/// Typed options of `modelRegistry.complete`.
#[derive(Debug, Clone, Default)]
pub struct CompleteOptions {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// Optional cancellation of the in-flight completion.
    pub signal: Option<CancellationHandle>,
}

/// A session entry looked up by id. The entry body stays JSON: session tree
/// rows are the canonical JSON representation shared with the JS face.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryRef {
    pub id: Arc<str>,
    pub entry: Arc<JsonValue>,
}

/// System-prompt construction options (upstream
/// `BuildSystemPromptOptions`, the subset the host keeps).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptOptions {
    pub cwd: Option<Arc<Path>>,
    pub custom_prompt: Option<Arc<str>>,
}

/// Options of `ctx.newSession`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewSessionOptions {
    pub parent_session: Option<Arc<str>>,
}

/// Where `ctx.fork` branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    /// Branch at the entry's parent (upstream default).
    Before,
    /// Branch at the entry itself.
    At,
}

impl ForkPosition {
    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::At => "at",
        }
    }
}

/// Options of `ctx.fork`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForkOptions {
    pub position: Option<ForkPosition>,
}

/// Options of `ctx.navigateTree`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavigateTreeOptions {
    pub summarize: bool,
    pub custom_instructions: Option<String>,
    pub replace_instructions: bool,
    pub label: Option<String>,
}

/// One status-line cell: `(key, text)`.
pub type StatusEntry = (Arc<str>, Arc<str>);

/// Snapshot of the native status cells, in insertion order.
pub type StatusSnapshot = Arc<[StatusEntry]>;

/// Cancellation handle over the session's abort flag (the Rust counterpart
/// of `ctx.signal`).
///
/// Parity limit (audit risk R9): the underlying flag is per-runtime, not
/// per-run, exactly like `ctx.signal`.
#[derive(Debug, Clone, Default)]
pub struct CancellationHandle(AbortSignal);

impl CancellationHandle {
    /// A fresh, un-aborted handle.
    pub fn new() -> Self {
        Self(AbortSignal::new())
    }

    /// Whether cancellation was requested.
    pub fn is_aborted(&self) -> bool {
        self.0.is_aborted()
    }

    /// Request cancellation and wake every waiter.
    pub fn abort(&self) {
        self.0.abort();
    }

    /// Resolve when cancellation is requested.
    pub fn cancelled(&self) -> ServiceFuture<'_, ()> {
        let signal = self.0.clone();
        Box::pin(async move { signal.cancelled().await })
    }
}

/// A subscription token for `pi.events` / `ui.onTerminalInput`.
///
/// The token is explicit (call [`Subscription::cancel`]); dropping it does
/// NOT unsubscribe, matching upstream's explicit unsubscribe function.
#[derive(Clone)]
pub struct Subscription {
    cancel: Arc<dyn Fn() + Send + Sync>,
}

impl Subscription {
    /// Build a subscription from its cancellation callback.
    pub fn new(cancel: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cancel: Arc::new(cancel),
        }
    }

    /// A subscription that does nothing (the default for absent services).
    pub fn noop() -> Self {
        Self {
            cancel: Arc::new(|| {}),
        }
    }

    /// Unsubscribe. Idempotence is the service's responsibility.
    pub fn cancel(&self) {
        (self.cancel)();
    }
}

impl Default for Subscription {
    fn default() -> Self {
        Self::noop()
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Subscription")
    }
}

// ---------------------------------------------------------------------------
// Provider auth hooks
// ---------------------------------------------------------------------------

/// Auth material produced by a native provider-auth hook.
///
/// Shape mirrors the project's existing credential model instead of
/// introducing a parallel one: [`crate::ai::auth::OAuthCredential`] is
/// `{refresh, access, expires, extra}`, and the api-key credential is a single
/// key. A hook returns either form:
///
/// - OAuth: set [`Self::access_token`] and [`Self::refresh_token`] (and
///   [`Self::expires`]); the credential is persisted as
///   `StoredCredential::OAuth`.
/// - API key: set [`Self::api_key`]; the credential is persisted as
///   `StoredCredential::ApiKey`.
///
/// The `expires` field name is deliberate: `login_and_store`'s existing
/// callers (CLI and TUI) read `credential.expires`, and reusing the name keeps
/// that public surface compiling while reusing the same epoch-millisecond
/// semantics as `OAuthCredential::expires`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderAuth {
    /// Stored api-key credential (`ApiKeyCredential::key`).
    pub api_key: Option<String>,
    /// OAuth access token (`OAuthCredential::access`).
    pub access_token: Option<String>,
    /// OAuth refresh token (`OAuthCredential::refresh`).
    pub refresh_token: Option<String>,
    /// Expiry in epoch milliseconds (`OAuthCredential::expires`); `0` for
    /// non-expiring api-key auth.
    pub expires: i64,
    /// OAuth flow-specific extra fields (preserved verbatim on the wire).
    pub extra: BTreeMap<String, JsonValue>,
}

impl ProviderAuth {
    /// API-key auth.
    pub fn api_key(key: impl Into<String>) -> Self {
        Self {
            api_key: Some(key.into()),
            ..Self::default()
        }
    }

    /// OAuth auth.
    pub fn oauth(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires: i64,
    ) -> Self {
        Self {
            access_token: Some(access_token.into()),
            refresh_token: Some(refresh_token.into()),
            expires,
            ..Self::default()
        }
    }

    /// Project an existing OAuth credential into the seam type.
    pub fn from_oauth(credential: &OAuthCredential) -> Self {
        Self {
            api_key: None,
            access_token: Some(credential.access.clone()),
            refresh_token: Some(credential.refresh.clone()),
            expires: credential.expires,
            extra: credential.extra.clone(),
        }
    }

    /// Project an existing stored credential into the seam type.
    pub fn from_stored(credential: &StoredCredential) -> Self {
        match credential {
            StoredCredential::ApiKey(credential) => Self {
                api_key: credential.key.clone(),
                ..Self::default()
            },
            StoredCredential::OAuth(credential) => Self::from_oauth(credential),
        }
    }

    /// The OAuth credential this auth represents, when it carries both
    /// tokens. `None` for api-key-only auth.
    pub fn as_oauth(&self) -> Option<OAuthCredential> {
        let access = self.access_token.as_ref()?;
        let refresh = self.refresh_token.as_ref()?;
        Some(OAuthCredential {
            refresh: refresh.clone(),
            access: access.clone(),
            expires: self.expires,
            extra: self.extra.clone(),
        })
    }
}

/// Typed failure of a native provider-auth hook.
///
/// `Unavailable` is the hook's explicit "this flow is not implemented here"
/// signal: the caller falls back to the builtin/JS flow without treating it
/// as a failure. `Failed` and `Cancelled` are terminal and leave the stored
/// credential untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuthError {
    /// The hook ran and failed.
    Failed(String),
    /// This host/hook cannot run the requested flow; fall back.
    Unavailable,
    /// The user cancelled the flow.
    Cancelled,
}

impl std::fmt::Display for ProviderAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(error) => f.write_str(error),
            Self::Unavailable => f.write_str("native provider auth hook is unavailable"),
            Self::Cancelled => f.write_str("provider login was cancelled"),
        }
    }
}

impl std::error::Error for ProviderAuthError {}

/// Host interaction surface handed to [`ProviderAuthHooks::login`].
///
/// It borrows the host's existing [`AuthInteraction`] and [`LoginOptions`]
/// rather than owning them: both login hosts (CLI and TUI) hold the
/// interaction as a stack-local value for the duration of the call, and
/// requiring every `AuthInteraction` implementation to become `Arc`-shared
/// would change that public contract. The hook future is therefore tied to
/// the interaction borrow; the registry itself stores only `'static` hooks.
#[derive(Clone, Copy)]
pub struct AuthPromptContext<'a> {
    /// Prompt/notify surface the hook uses for user interaction.
    pub interaction: &'a dyn AuthInteraction,
    /// App-supplied login options (device id, agent name).
    pub options: &'a LoginOptions,
}

/// Typed native provider-auth seam (the last JS-only provider capability,
/// audit §1.9 / §5.3.5).
///
/// A Rust provider registers hooks through
/// [`ProviderService::register_auth_hooks`]. Login/refresh/logout prefer the
/// native hook over the builtin OAuth flow and over a JS `registerProvider`
/// OAuth callback; when the hook answers [`ProviderAuthError::Unavailable`]
/// the caller runs exactly one fallback flow (JS is never invoked twice).
///
/// Supported user interaction is exactly what [`AuthPromptContext`] exposes:
/// the host's text/secret/select/manual-code prompts plus the four
/// [`crate::ai::auth::AuthEvent`] notifications (info, auth URL, device code,
/// progress). A hook that needs a bespoke UI reports `Unavailable`.
pub trait ProviderAuthHooks: Send + Sync {
    /// Run the provider login and return the credential material.
    ///
    /// The future borrows `self` and the prompt context; see
    /// [`AuthPromptContext`] for why it is not `'static`.
    fn login<'a>(
        &'a self,
        context: AuthPromptContext<'a>,
    ) -> ServiceFuture<'a, Result<ProviderAuth, ProviderAuthError>>;

    /// Refresh `current` auth. The returned auth replaces the stored
    /// credential only on `Ok`; `Failed`/`Cancelled` preserve it.
    fn refresh(
        &self,
        current: &ProviderAuth,
    ) -> ServiceFuture<'static, Result<ProviderAuth, ProviderAuthError>>;

    /// Revoke/forget any hook-owned session state. The credential store
    /// removal stays with the caller and runs after this succeeds (or when
    /// the hook reports `Unavailable`).
    fn logout(&self) -> ServiceFuture<'static, Result<(), ProviderAuthError>> {
        Box::pin(async { Ok(()) })
    }

    /// Whether the provider is subscription-backed (login UI hint).
    fn is_subscription(&self) -> bool {
        false
    }
}

/// Hard bound on registered provider-auth hooks. Registration is last-wins
/// for an existing provider id; inserting a new id at the bound evicts the
/// oldest registration (insertion order), so a plugin that registers per
/// provider cannot grow the process-wide table without limit.
pub const MAX_PROVIDER_AUTH_HOOKS: usize = 64;

/// Internal insertion-ordered slot table.
#[derive(Default)]
struct ProviderAuthHookSlots {
    hooks: BTreeMap<String, Arc<dyn ProviderAuthHooks>>,
    order: VecDeque<String>,
}

impl ProviderAuthHookSlots {
    fn register(&mut self, provider_id: &str, hooks: Arc<dyn ProviderAuthHooks>) {
        if !self.hooks.contains_key(provider_id) {
            while self.hooks.len() >= MAX_PROVIDER_AUTH_HOOKS {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.hooks.remove(&oldest);
            }
            self.order.push_back(provider_id.to_owned());
        }
        // Last registration for a provider wins (it keeps its queue slot).
        self.hooks.insert(provider_id.to_owned(), hooks);
    }

    fn unregister(&mut self, provider_id: &str) -> bool {
        let removed = self.hooks.remove(provider_id).is_some();
        if removed {
            self.order.retain(|slot| slot != provider_id);
        }
        removed
    }

    fn unregister_if(&mut self, provider_id: &str, hooks: &Arc<dyn ProviderAuthHooks>) -> bool {
        let owned = self
            .hooks
            .get(provider_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, hooks));
        if owned {
            self.unregister(provider_id)
        } else {
            false
        }
    }

    fn get(&self, provider_id: &str) -> Option<Arc<dyn ProviderAuthHooks>> {
        self.hooks.get(provider_id).cloned()
    }
}

/// Process-wide native provider-auth hook table.
///
/// The table is process-wide because auth entry points (`pir auth login`,
/// `/login`, token refresh) run outside any one session and must find a hook
/// registered by the live plugin host. It stores only `Arc<dyn
/// ProviderAuthHooks>`; it never captures an `AgentSession` or a
/// `PluginHost`, so a hook that needs session state must hold its own `Weak`
/// reference. [`ProviderService::unregister_auth_hooks`] removes an entry;
/// the live service additionally removes only the hooks it still owns when
/// its session/plugin context is dropped.
#[derive(Default)]
pub struct ProviderAuthHookRegistry {
    slots: Mutex<ProviderAuthHookSlots>,
}

impl ProviderAuthHookRegistry {
    /// An empty registry (the process-global one is
    /// [`provider_auth_hook_registry`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Register/replace the hook for `provider_id` (last wins).
    pub fn register(&self, provider_id: &str, hooks: Arc<dyn ProviderAuthHooks>) {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register(provider_id, hooks);
    }

    /// Remove the hook for `provider_id`; returns whether one was present.
    pub fn unregister(&self, provider_id: &str) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unregister(provider_id)
    }

    /// Remove the hook only when it is still the exact `hooks` registration
    /// (session-teardown cleanup must not unregister a newer session's hook).
    pub fn unregister_if(&self, provider_id: &str, hooks: &Arc<dyn ProviderAuthHooks>) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unregister_if(provider_id, hooks)
    }

    /// Look up the hook registered for `provider_id`.
    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn ProviderAuthHooks>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider_id)
    }

    /// Number of registered hooks (tests/diagnostics).
    pub fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hooks
            .len()
    }

    /// Whether no hook is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for ProviderAuthHookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAuthHookRegistry")
            .field("len", &self.len())
            .finish()
    }
}

fn provider_auth_hook_registry_cell() -> &'static OnceLock<ProviderAuthHookRegistry> {
    static REGISTRY: OnceLock<ProviderAuthHookRegistry> = OnceLock::new();
    &REGISTRY
}

/// The process-wide provider-auth hook table.
pub fn provider_auth_hook_registry() -> &'static ProviderAuthHookRegistry {
    provider_auth_hook_registry_cell().get_or_init(ProviderAuthHookRegistry::new)
}

/// Look up the native auth hook registered for `provider_id`.
///
/// This is the lookup auth code uses ("is there a native hook for provider
/// X?") without downcasting a service object. It is equivalent to
/// [`ProviderService::auth_hooks`] on the live service.
pub fn native_provider_auth_hooks(provider_id: &str) -> Option<Arc<dyn ProviderAuthHooks>> {
    provider_auth_hook_registry().get(provider_id)
}

// ---------------------------------------------------------------------------
// Renderer / widget / completion traits
// ---------------------------------------------------------------------------

/// Renderer registry bucket (`tui::renderers::RendererKind` counterpart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererKind {
    Message,
    Entry,
    Markdown,
    Tool,
    Widget,
    Footer,
    Header,
}

impl RendererKind {
    /// The wire bucket key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Entry => "entry",
            Self::Markdown => "markdown",
            Self::Tool => "tool",
            Self::Widget => "widget",
            Self::Footer => "footer",
            Self::Header => "header",
        }
    }
}

/// Native message renderer (upstream `registerMessageRenderer`).
pub trait MessageRenderer: Send + Sync {
    /// Render one message; an unhandled outcome keeps the default rendering.
    fn render(&self, message: &JsonValue, width: usize) -> RendererOutcome;
}

/// Native entry renderer (upstream `registerEntryRenderer`).
pub trait EntryRenderer: Send + Sync {
    /// Render one session entry.
    fn render(&self, entry: &JsonValue, width: usize) -> RendererOutcome;
}

/// Native markdown transformer (upstream `registerMarkdownTransformer`).
pub trait MarkdownTransformer: Send + Sync {
    /// Transform markdown text; `None` keeps the input unchanged.
    fn transform(&self, markdown: &str) -> Option<String>;
}

/// One tool render request handed to a native resolver.
#[derive(Debug, Clone, Copy)]
pub struct ToolRenderRequest<'a> {
    pub tool_name: &'a str,
    pub args: &'a JsonValue,
    pub result: &'a JsonValue,
    pub is_error: bool,
    pub width: usize,
}

/// Native tool renderer resolver (upstream `registerToolRenderer`).
pub trait ToolRendererResolver: Send + Sync {
    /// Render the call; an unhandled outcome delegates to the next resolver.
    fn render(&self, request: &ToolRenderRequest<'_>) -> RendererOutcome;

    /// Render the call with the resolver's upstream `renderShell` choice
    /// (`ToolRenderers`, extensions/types.ts:672). The default keeps the
    /// legacy behavior: a plain [`RendererOutcome`] with no shell metadata,
    /// so existing resolvers compile unchanged.
    fn render_with_shell(&self, request: &ToolRenderRequest<'_>) -> RendererCallOutcome {
        RendererCallOutcome::from_outcome(self.render(request))
    }
}

/// The reserved `setStatus` key the JS prelude's `tui.requestRender()` stub
/// pushes; the TUI drain intercepts it as a factory repaint signal (it never
/// reaches the status line). Native factories receive the same hook through
/// [`FactoryRenderContext::request_render`].
pub const FACTORY_RENDER_REQUEST_KEY: &str = "__pir_request_render";

/// `Notification::DialogRequest::kind` carrying an externally supplied
/// overlay component (upstream `ctx.ui.custom`).
///
/// Both faces produce this one kind: the JS prelude's `ctx.ui.custom` calls
/// `__pir_host.uiDialog("custom", {component})`, and [`UiService::custom`]
/// pushes the same request natively. The TUI resolves `options.component` to
/// the face that owns it (a `native:<id>` component in the host-scoped
/// registry, otherwise the prelude's component key) and admits it through the
/// ordinary overlay path.
pub const CUSTOM_OVERLAY_DIALOG_KIND: &str = "custom";

/// The reserved `setStatus` key an external overlay component uses to talk to
/// the host: `{"component": "<key>", "op": "repaint"}` for
/// `tui.requestRender()`, `{"component": "<key>", "op": "done",
/// "result": <json>}` for the factory's `done(result)` callback. The TUI
/// drain intercepts it like [`FACTORY_RENDER_REQUEST_KEY`]; it never reaches
/// the status line.
pub const CUSTOM_OVERLAY_EVENT_KEY: &str = "__pir_custom_overlay";

/// The `__pir_callRenderer` kind an external overlay component is rendered
/// and driven through (the prelude's `customOverlays` bucket, keyed by the
/// component id allocated when `ctx.ui.custom` opened it). One channel serves
/// render and input: the request `payload.op` is `"render"` or `"input"`.
pub const CUSTOM_OVERLAY_RENDER_KIND: &str = "customOverlays";

/// One native factory render request.
///
/// Carries the same data the JS factory pipeline receives:
///
/// - `width`: the frame width (`component.render(width)` in the JS bridge);
/// - `theme`: the live theme snapshot (the JS `pirWidgetTheme()` argument);
/// - `footer_data`: the `ReadonlyFooterDataProvider` snapshot for the footer
///   slot (the JS footer factory's third argument); `None` for widgets,
///   headers and a footer render without a snapshot;
/// - `request_render`: the `tui.requestRender()` stub. Calling it queues the
///   reserved repaint signal on the shared presentation queue, so the owning
///   slot re-renders on the next TUI tick exactly like a JS component's
///   `requestRender()`.
pub struct FactoryRenderContext<'a> {
    pub width: usize,
    pub theme: Option<&'a ThemeSnapshot>,
    pub footer_data: Option<&'a JsonValue>,
    pub request_render: Option<&'a dyn Fn()>,
}

/// Native widget factory (upstream `ctx.ui.setWidget(key, factory)`).
pub trait WidgetFactory: Send + Sync {
    /// Render the widget's lines for one frame.
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String>;
}

/// Native footer/header factory (upstream `setFooter` / `setHeader`).
pub trait BarFactory: Send + Sync {
    /// Render the bar's lines for one frame.
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// Externally supplied overlay components (`ctx.ui.custom`)
//
// Upstream `ctx.ui.custom(factory, options)` (extensions/types.ts:213-229)
// builds a pi-tui `Component` and gives it keyboard focus until the factory's
// `done(result)` callback runs. The component cannot cross the JSON face, so
// the native face mirrors it with the two traits below and the JS face keeps
// its component in the prelude, rendered through the existing
// `__pir_callRenderer` round trip. BOTH faces converge on ONE TUI mechanism:
// the overlay slot in `tui::terminal` (`ExternalOverlay`), which is admitted,
// painted, key-routed and settled like every other extension overlay.
// ---------------------------------------------------------------------------

/// One externally supplied overlay component (the `Component` upstream's
/// `custom` factory returns).
pub trait OverlayComponent: Send + Sync {
    /// Render the component's lines for one frame. The TUI paints them in the
    /// editor slot, exactly where a selector overlay renders.
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String>;

    /// Deliver one canonical terminal input chunk (the bytes upstream's
    /// focused component receives through `handleInput(data)`).
    fn handle_input(&self, _data: &str) {}

    /// Release resources when the overlay closes. Called exactly once, by
    /// whichever path removes the registration.
    fn dispose(&self) {}
}

/// Native `ctx.ui.custom` factory: builds the component for one overlay.
///
/// `done` is the factory's completion callback (upstream's fourth factory
/// argument): calling it with a value closes the overlay and resolves
/// [`UiService::custom`] with that value. `None` from `create` means the
/// factory declined (an unusable component); the dialog fails closed with an
/// actionable error instead of showing an empty overlay. A panicking factory
/// is isolated and treated the same way.
pub trait OverlayComponentFactory: Send + Sync {
    fn create(&self, done: OverlayDone) -> Option<Arc<dyn OverlayComponent>>;
}

/// The completion callback handed to a native custom-overlay factory
/// (`Arc` so both the factory and its component may hold it).
pub type OverlayDone = Arc<dyn Fn(JsonValue) + Send + Sync>;

/// Native raw-terminal-input handler (upstream `onTerminalInput`).
pub trait TerminalInputHandler: Send + Sync {
    /// Inspect one input chunk and decide pass/consume/replace.
    fn handle(&self, input: &str) -> TerminalInputAction;
}

/// One completion request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionContext {
    /// The word prefix being completed.
    pub prefix: String,
    /// The full editor line.
    pub line: String,
    /// Cursor byte offset in `line`.
    pub cursor: usize,
}

/// One completion suggestion (upstream `AutocompleteItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// Inserted value.
    pub value: String,
    /// Label shown in the popup.
    pub label: String,
    /// Optional hint.
    pub description: Option<String>,
}

impl AutocompleteItem {
    /// Build an item with no description.
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    /// Attach a hint.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Native command-argument completion provider (upstream
/// `getArgumentCompletions`).
pub trait ArgumentCompletions: Send + Sync {
    /// Suggestions for `prefix`.
    fn complete(&self, prefix: &str) -> ServiceFuture<'static, Vec<AutocompleteItem>>;
}

/// Native autocomplete provider (upstream `addAutocompleteProvider`).
pub trait AutocompleteProvider: Send + Sync {
    /// Suggestions for one completion context.
    fn suggest(&self, context: &CompletionContext)
        -> ServiceFuture<'static, Vec<AutocompleteItem>>;
}

// ---------------------------------------------------------------------------
// Service traits
// ---------------------------------------------------------------------------

/// Session snapshot/meta reads, queued session actions, and `wait_for_idle`.
///
/// The synchronous reads are the Rust face of the `ctx` facade and
/// `ctx.sessionManager`; they must be served from cached state (an `Arc`
/// clone at most), never by serializing/parsing on the call path.
pub trait SessionService: Send + Sync {
    // -- snapshot / meta reads (sync, Arc/borrow) ---------------------------

    /// The last host-pushed snapshot.
    fn snapshot(&self) -> Option<Arc<SessionContextSnapshot>> {
        None
    }

    /// Current session id.
    fn session_id(&self) -> Option<Arc<str>> {
        None
    }

    /// Current session file path.
    fn session_path(&self) -> Option<Arc<Path>> {
        None
    }

    /// Working directory (`ctx.sessionManager.getCwd()`).
    fn cwd(&self) -> Option<Arc<Path>> {
        None
    }

    /// The host mode (`ctx.mode`).
    fn mode(&self) -> ExtensionMode {
        ExtensionMode::Headless
    }

    /// Whether a UI is attached (`ctx.hasUI`).
    fn has_ui(&self) -> bool {
        false
    }

    /// Whether no run is in flight (`ctx.isIdle()`).
    fn is_idle(&self) -> bool {
        true
    }

    /// Whether project-local trust is active.
    fn is_project_trusted(&self) -> bool {
        false
    }

    /// Whether steering/follow-up messages are queued.
    fn has_pending_messages(&self) -> bool {
        false
    }

    /// Session display name.
    fn session_name(&self) -> Option<Arc<str>> {
        None
    }

    /// Current model, cached as a typed value by the host.
    fn model(&self) -> Option<Arc<Model>> {
        None
    }

    /// Scoped models (`ctx.scopedModels`).
    fn scoped_models(&self) -> Arc<[ScopedModel]> {
        Arc::from(Vec::new())
    }

    /// Current thinking level.
    fn thinking_level(&self) -> ThinkingLevel {
        ThinkingLevel::Off
    }

    /// Context-window usage, when known.
    fn context_usage(&self) -> Option<ContextUsage> {
        None
    }

    /// Effective system prompt.
    fn system_prompt(&self) -> Arc<str> {
        Arc::from("")
    }

    /// System-prompt construction options.
    fn system_prompt_options(&self) -> SystemPromptOptions {
        SystemPromptOptions::default()
    }

    /// Settings snapshot (`ctx.getSettings()`; inherently JSON).
    fn settings(&self) -> Arc<JsonValue> {
        Arc::new(JsonValue::Null)
    }

    /// Resolved slash commands from the live registry.
    fn commands(&self) -> Arc<[CommandInfo]> {
        Arc::from(Vec::new())
    }

    /// All registered tools from the live registry.
    fn all_tools(&self) -> Arc<[ToolInfo]> {
        Arc::from(Vec::new())
    }

    /// Active tool names (`ctx.getActiveTools()`).
    fn active_tools(&self) -> Arc<[Arc<str>]> {
        Arc::from(Vec::new())
    }

    /// Whether the session's tool set derives from the DEFAULT tools rather
    /// than an explicit `--tools` / `--no-tools` / `--no-builtin-tools`
    /// selection (upstream `AgentSessionConfig.usesDefaultTools`).
    ///
    /// A plugin that activates tools on registration (the `mcp` builtin's
    /// direct-exposure bridges) consults this before unioning them into the
    /// active set: default-tools sessions must keep every registered tool
    /// reachable even when an extension's `setActiveTools` snapshot omitted
    /// them, while an explicit selection stays authoritative. The default
    /// `true` matches a host that never resolved a selection.
    fn uses_default_tools(&self) -> bool {
        true
    }

    /// Tool names a default-tools selection DELETED with a `-name` entry
    /// (`--exclude-tools`, or a `-name` in a `--tools` modifier list).
    ///
    /// An exclusion is the user's most explicit statement about a tool, so it
    /// outranks a plugin's registration-time activation even on a
    /// default-tools session: the `mcp` builtin never re-adds a name listed
    /// here. Empty for an explicit selection (nothing may be added there in the
    /// first place).
    fn default_tool_exclusions(&self) -> Arc<[Arc<str>]> {
        Arc::from(Vec::new())
    }

    // -- session-manager snapshot reads (sync, Arc/borrow) ------------------

    /// Serialized session entries (`snapshot.entries`); the SAME `Arc` the
    /// host pushed, so Rust readers never re-serialize.
    fn entries(&self) -> Option<Arc<JsonValue>> {
        None
    }

    /// Serialized leaf-path branch.
    fn branch(&self) -> Option<Arc<JsonValue>> {
        None
    }

    /// Pre-rendered session tree text.
    fn tree(&self) -> Arc<str> {
        Arc::from("")
    }

    /// Serialized session projection (`{entries, messages}`).
    fn projection(&self) -> Option<Arc<JsonValue>> {
        None
    }

    /// Serialized session header.
    fn header(&self) -> Option<Arc<JsonValue>> {
        None
    }

    /// Leaf entry id.
    fn leaf_id(&self) -> Option<Arc<str>> {
        None
    }

    /// Label of one entry, when set.
    fn label(&self, _entry_id: &str) -> Option<Arc<str>> {
        None
    }

    /// One entry by id. Implementations MAY use an O(1) index; the default
    /// contract does not guarantee it.
    fn entry(&self, _entry_id: &str) -> Option<EntryRef> {
        None
    }

    // -- cancellation -------------------------------------------------------

    /// Cancellation handle over the session abort flag.
    fn signal(&self) -> CancellationHandle {
        CancellationHandle::new()
    }

    // -- writes / queued actions (sync, same queue as the JS face) ----------

    /// `pi.setSessionName`.
    fn set_session_name(&self, _name: Option<&str>) {}

    /// `pi.setLabel`.
    fn set_label(&self, _entry_id: &str, _label: Option<&str>) {}

    /// `pi.setModel`. Returns the auth-gated acceptance exactly like the JS
    /// host function; the change is queued for the session host.
    fn set_model(&self, _model: &Model) -> bool {
        false
    }

    /// `pi.setThinkingLevel`.
    fn set_thinking_level(&self, _level: ThinkingLevel) {}

    /// `pi.setActiveTools`.
    fn set_active_tools(&self, _names: &[Arc<str>]) {}

    /// `ctx.compact`.
    fn compact(&self, _custom_instructions: Option<&str>) {}

    /// `ctx.abort()`.
    fn abort(&self) {}

    /// `ctx.shutdown()`.
    fn shutdown(&self) {}

    /// `ctx.newSession`.
    fn new_session(&self, _options: NewSessionOptions) {}

    /// `ctx.fork`.
    fn fork(&self, _entry_id: Option<&str>, _options: ForkOptions) {}

    /// `ctx.navigateTree`.
    fn navigate_tree(&self, _target_id: &str, _options: NavigateTreeOptions) {}

    /// `ctx.switchSession`.
    fn switch_session(&self, _path: &Path) {}

    /// `ctx.reload()`.
    fn reload(&self) {}

    // -- async --------------------------------------------------------------

    /// Resolve when no run is in flight (`ctx.waitForIdle`).
    fn wait_for_idle(&self) -> ServiceFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Message injection / steering (upstream `pi.sendMessage`,
/// `pi.sendUserMessage`, `pi.appendEntry`).
///
/// Synchronous by design: the JS face enqueues into the same bounded action
/// queue and returns before the host applies the message (audit §2.3).
pub trait MessageService: Send + Sync {
    /// Queue a custom message.
    fn send_message(&self, _message: CustomMessage, _options: SendMessageOptions) {}

    /// Queue a user message.
    fn send_user_message(&self, _content: UserContent, _options: SendUserMessageOptions) {}

    /// Append a custom session entry; `data` is inherently JSON.
    fn append_entry(&self, _custom_type: &str, _data: Option<JsonValue>) {}
}

/// Tool execution (`ctx.executeTool`), exec (`pi.exec`), and in-flight tool
/// updates.
pub trait ToolService: Send + Sync {
    /// Tools callable from `ctx.tools`, as registry definitions.
    fn callable_tools(&self) -> Arc<[ToolInfo]> {
        Arc::from(Vec::new())
    }

    /// Execute a nested tool call. `caller_id` is the calling tool's call id
    /// (the child id becomes `<caller_id>/<n>`); `args` is the tool's JSON
    /// input, moved in without re-serialization.
    fn execute_tool(
        &self,
        _caller_id: &str,
        _name: &str,
        _args: JsonValue,
        _options: ExecuteToolOptions,
    ) -> ServiceFuture<'static, ServiceResult<NestedToolOutcome>> {
        Box::pin(async { Err(PluginServiceError::unavailable("tools")) })
    }

    /// Run a subprocess (`pi.exec`).
    fn exec(
        &self,
        _command: &str,
        _args: Vec<String>,
        _options: ExecOptions,
    ) -> ServiceFuture<'static, ServiceResult<ExecResult>> {
        Box::pin(async { Err(PluginServiceError::unavailable("tools")) })
    }

    /// Emit a `tool_execution_update` for a running call.
    fn update(&self, _call: &ToolCallRef, _partial: JsonValue) {}
}

/// UI surface: notices, dialogs, working state, widgets/bars, editor, theme.
pub trait UiService: Send + Sync {
    /// `ctx.ui.notify`.
    fn notify(&self, _level: NotifyLevel, _message: &str) {}

    /// `ctx.ui.select`.
    fn select(
        &self,
        _title: &str,
        _options: &[SelectOption],
        _dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        Box::pin(async { Err(PluginServiceError::unavailable("ui")) })
    }

    /// `ctx.ui.confirm`.
    fn confirm(
        &self,
        _title: &str,
        _message: &str,
        _options: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<bool>>> {
        Box::pin(async { Err(PluginServiceError::unavailable("ui")) })
    }

    /// `ctx.ui.input`.
    fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _options: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        Box::pin(async { Err(PluginServiceError::unavailable("ui")) })
    }

    /// `ctx.ui.editor`.
    fn editor(
        &self,
        _title: &str,
        _prefill: Option<&str>,
        _options: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        Box::pin(async { Err(PluginServiceError::unavailable("ui")) })
    }

    /// `ctx.ui.custom(factory, options)` (upstream types.ts:213-229): show a
    /// caller-supplied component with keyboard focus and resolve with the
    /// value the component's `done(result)` produced.
    ///
    /// The overlay rides the SAME admission path as
    /// [`UiService::select`]/`confirm`/`input`/`editor` — one dialog slot,
    /// one bounded queue, one deadline, one settlement funnel — so a custom
    /// overlay opened while another modal is up queues behind it instead of
    /// stacking a second focus system.
    ///
    /// Resolution contract (upstream `showExtensionCustom`,
    /// interactive-mode.ts:2966-3026): `done(value)` resolves `Some(value)`;
    /// a dismissal (Esc/`Ctrl+C`, the overlay deadline, a session switch)
    /// resolves `Ok(None)` — JS `undefined`. A factory that cannot build a
    /// usable component, or one that throws, fails with
    /// [`PluginServiceError::Failed`]; the caller's await never wedges.
    fn custom(
        &self,
        _factory: Arc<dyn OverlayComponentFactory>,
        _options: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<JsonValue>>> {
        Box::pin(async { Err(PluginServiceError::unavailable("ui")) })
    }

    /// `ctx.ui.setWorkingMessage`.
    fn set_working_message(&self, _message: Option<&str>) {}

    /// `ctx.ui.setWorkingVisible`.
    fn set_working_visible(&self, _visible: bool) {}

    /// `ctx.ui.setWorkingIndicator`.
    fn set_working_indicator(&self, _options: Option<WorkingIndicator>) {}

    /// `ctx.ui.setHiddenThinkingLabel`.
    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}

    /// `ctx.ui.setTitle`.
    fn set_title(&self, _title: &str) {}

    /// `ctx.ui.setWidget(key, lines, placement)`.
    ///
    /// A static registration replaces a native factory previously registered
    /// for the same key (upstream `setWidget` last-wins).
    fn set_widget_static(&self, _key: &str, _lines: &[String], _placement: WidgetPlacement) {}

    /// `ctx.ui.setWidget(key, factory, placement)`.
    ///
    /// The factory is owned by the native registry and resolved BEFORE the JS
    /// `widgetRenderers` entry for the same key (native wins at the same
    /// slot). The last native registration for a key wins. Reads resolve
    /// through [`RendererService::render_widget`].
    fn set_widget(
        &self,
        _key: &str,
        _factory: Arc<dyn WidgetFactory>,
        _placement: WidgetPlacement,
    ) {
    }

    /// `ctx.ui.setWidget(key, undefined)` — remove a native factory widget.
    ///
    /// Only the native registration is retired. If a JS factory is also
    /// registered for the key, the TUI falls back to it; otherwise the widget
    /// (and any static text under the key) disappears. A clear for a key the
    /// native face does not own is a no-op: it never retires a JS
    /// registration.
    fn clear_widget(&self, _key: &str) {}

    /// `ctx.ui.setFooter`.
    ///
    /// `Some` registers/replaces the native footer factory (resolved before a
    /// JS `setFooter` factory); `None` retires the native registration only,
    /// revealing a JS footer when one is registered and otherwise restoring
    /// the built-in footer.
    fn set_footer(&self, _factory: Option<Arc<dyn BarFactory>>) {}

    /// `ctx.ui.setHeader`. Same precedence/removal contract as
    /// [`UiService::set_footer`].
    fn set_header(&self, _factory: Option<Arc<dyn BarFactory>>) {}

    /// `ctx.ui.pasteToEditor`.
    fn paste_to_editor(&self, _text: &str) {}

    /// `ctx.ui.setEditorText`.
    fn set_editor_text(&self, _text: &str) {}

    /// `ctx.ui.getEditorText`.
    fn editor_text(&self) -> Arc<str> {
        Arc::from("")
    }

    /// Active theme snapshot.
    fn theme(&self) -> Option<Arc<ThemeSnapshot>> {
        None
    }

    /// All known themes.
    fn themes(&self) -> Arc<[ThemeInfo]> {
        Arc::from(Vec::new())
    }

    /// `ctx.ui.setTheme`.
    fn set_theme(&self, _name: &str) -> ThemeSetResult {
        ThemeSetResult::Unavailable
    }

    /// `ctx.ui.getToolsExpanded`.
    fn tools_expanded(&self) -> bool {
        false
    }

    /// `ctx.ui.setToolsExpanded`.
    fn set_tools_expanded(&self, _expanded: bool) {}

    /// `ctx.ui.onTerminalInput`.
    fn on_terminal_input(&self, _handler: Arc<dyn TerminalInputHandler>) -> Subscription {
        Subscription::noop()
    }
}

/// Provider / MCP / virtual-model registration and `modelRegistry.complete`.
pub trait ProviderService: Send + Sync {
    /// `pi.registerProvider`. `config` is the upstream provider config
    /// (inherently JSON: models, hooks, headers).
    fn register_provider(&self, _id: &str, _config: JsonValue) {}

    /// `pi.unregisterProvider`.
    fn unregister_provider(&self, _id: &str) {}

    /// Register a Rust provider implementation (the typed `streamSimple` /
    /// OAuth seam).
    fn register_provider_impl(&self, _provider: ProviderBox) {}

    /// Register the typed native auth hook for a provider (login/refresh/
    /// logout). Last registration for the same provider wins.
    ///
    /// The default wires the process-wide [`provider_auth_hook_registry`] so
    /// every `ProviderService` implementation exposes the same seam; the live
    /// session service overrides it to also track the registration for
    /// teardown.
    fn register_auth_hooks(&self, provider_id: &str, hooks: Arc<dyn ProviderAuthHooks>) {
        provider_auth_hook_registry().register(provider_id, hooks);
    }

    /// Remove the registered native auth hook for a provider.
    fn unregister_auth_hooks(&self, provider_id: &str) {
        let _ = provider_auth_hook_registry().unregister(provider_id);
    }

    /// Whether a native auth hook is registered for `provider_id` (the typed
    /// lookup auth code uses; no downcast).
    fn auth_hooks(&self, provider_id: &str) -> Option<Arc<dyn ProviderAuthHooks>> {
        provider_auth_hook_registry().get(provider_id)
    }

    /// `pi.registerMcpServer`; `config` is inherently JSON.
    fn register_mcp_server(&self, _name: &str, _config: JsonValue) {}

    /// `pi.unregisterMcpServer`.
    fn unregister_mcp_server(&self, _name: &str) {}

    /// Extension-registered MCP servers only (parity with the JS
    /// `getMcpServers`, not the config-file registry).
    fn mcp_servers(&self) -> Arc<[RegisteredMcpServer]> {
        Arc::from(Vec::new())
    }

    /// `pi.registerVirtualModel`; the definition is upstream wire JSON.
    fn register_virtual_model(&self, _definition: JsonValue) {}

    /// `pi.unregisterVirtualModel`.
    fn unregister_virtual_model(&self, _provider: &str, _id: &str) {}

    /// Physical model behind a virtual model, when one exists.
    fn physical_model(&self, _provider: &str, _id: &str) -> Option<Arc<Model>> {
        None
    }

    /// Whether the provider has configured auth (permissive-empty convention).
    fn has_auth(&self, _provider: &str) -> bool {
        false
    }

    /// `modelRegistry.complete`, bound to the session's completion
    /// capability: models other than the session's current one fail closed.
    fn complete(
        &self,
        _model: &Model,
        _context: Arc<ModelContext>,
        _options: CompleteOptions,
    ) -> ServiceFuture<'static, ServiceResult<AssistantMessage>> {
        Box::pin(async { Err(PluginServiceError::unavailable("providers")) })
    }
}

/// Native renderer registration, resolution, and TUI pre-checks.
///
/// Registration follows the JS face's ordering rules: the first registration
/// per custom type wins for message/entry renderers, markdown transformers
/// compose in registration order, and tool resolvers form a per-tool chain
/// (name-specific resolvers in registration order, then fallbacks) where an
/// unhandled outcome delegates to the next resolver.
///
/// Resolution methods return an [`RendererOutcome`] whose
/// [`RendererOutcome::is_unhandled`] answer means "keep the default / delegate
/// to the JS renderer". The TUI consumes the native registry BEFORE the JS
/// bridge for message/entry (native wins at the same key) and AFTER it for
/// tool/markdown (the JS load-order chain runs first, native registrations
/// compose after it) — see `tui::renderers`.
///
/// ## Factory widgets / footer / header precedence
///
/// Widget factories (`ctx.ui.setWidget(key, factory)`), the footer factory
/// (`setFooter`) and the header factory (`setHeader`) resolve with the SAME
/// rule as message/entry renderers: **native first at the same slot**. A
/// native registration therefore wins over a JS registration for the same
/// widget key / bar slot regardless of which face registered first, matching
/// the "native registers after JS" order the host binds its services in.
/// Removal is face-scoped: [`UiService::clear_widget`] and
/// `set_footer(None)` / `set_header(None)` retire only the native
/// registration. A still-registered JS factory for the slot renders again;
/// when no JS factory remains the TUI restores the built-in rendering
/// (the widget disappears / the built-in bar returns). A removal for a slot
/// the native face never owned is a no-op and never clears the JS
/// registration.
pub trait RendererService: Send + Sync {
    /// `pi.registerMessageRenderer`.
    fn register_message(&self, _custom_type: &str, _renderer: Arc<dyn MessageRenderer>) {}

    /// `pi.registerEntryRenderer`.
    fn register_entry(&self, _custom_type: &str, _renderer: Arc<dyn EntryRenderer>) {}

    /// `pi.registerMarkdownTransformer`; transformers compose in registration
    /// order.
    fn register_markdown(&self, _transformer: Arc<dyn MarkdownTransformer>) {}

    /// `pi.registerToolRenderer`; `None` registers the fallback resolver.
    fn register_tool(&self, _tool_name: Option<&str>, _resolver: Arc<dyn ToolRendererResolver>) {}

    /// Whether a renderer is registered for `(kind, key)` (the TUI's
    /// `renderer_registered` pre-check).
    ///
    /// `Markdown` ignores `key` (any registered transformer matches); `Tool`
    /// matches a tool-specific resolver or any fallback resolver. `Widget`
    /// matches a native factory registration for `key`; `Footer`/`Header`
    /// ignore `key` (one native factory slot each).
    fn has(&self, _kind: RendererKind, _key: &str) -> bool {
        false
    }

    /// Whether any native factory slot (widget, footer, header) is
    /// registered. The TUI uses this to avoid dirtying its factory caches
    /// when only the JS face has registrations.
    fn has_native_factories(&self) -> bool {
        false
    }

    /// Whether a native custom-overlay component is waiting for dialog `id`
    /// (see [`UiService::custom`]). The TUI checks this before treating a
    /// `kind: "custom"` [`DialogRequest`](crate::extensions::Notification)
    /// as renderable.
    fn has_overlay_component(&self, _id: u64) -> bool {
        false
    }

    /// Take the native custom-overlay component registered for dialog `id`,
    /// removing it from the registry (one registration, one overlay).
    ///
    /// `None` means the id is not a native overlay — the JS face, an already
    /// taken registration, or a cancelled request.
    fn take_overlay_component(&self, _id: u64) -> Option<Arc<dyn OverlayComponent>> {
        None
    }

    /// Placement of the native factory widget registered for `key`, when one
    /// exists.
    fn widget_placement(&self, _key: &str) -> Option<WidgetPlacement> {
        None
    }

    /// Render one native factory widget.
    ///
    /// `None` means "not handled natively": either no native factory is
    /// registered for `key`, or it panicked (panic-isolated, exactly like a
    /// panicking message renderer). The caller then delegates to the JS
    /// factory and finally to the built-in/empty rendering.
    fn render_widget(
        &self,
        _key: &str,
        _context: &FactoryRenderContext<'_>,
    ) -> Option<Vec<String>> {
        None
    }

    /// Render the native footer factory. `None` = no native registration or
    /// the factory panicked; the caller falls back to the JS footer, then the
    /// built-in footer.
    fn render_footer(&self, _context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        None
    }

    /// Render the native header factory. Same fallback contract as
    /// [`RendererService::render_footer`].
    fn render_header(&self, _context: &FactoryRenderContext<'_>) -> Option<Vec<String>> {
        None
    }

    /// Render one custom message through the native registry. Unhandled keeps
    /// the JS/default rendering.
    fn render_message(
        &self,
        _custom_type: &str,
        _message: &JsonValue,
        _width: usize,
    ) -> RendererOutcome {
        RendererOutcome::default()
    }

    /// Render one custom session entry through the native registry.
    fn render_entry(
        &self,
        _custom_type: &str,
        _entry: &JsonValue,
        _width: usize,
    ) -> RendererOutcome {
        RendererOutcome::default()
    }

    /// Apply every registered markdown transformer in registration order.
    /// `None` keeps the input unchanged.
    fn transform_markdown(&self, _markdown: &str) -> Option<String> {
        None
    }

    /// Number of registered markdown transformers (the TUI's cheap pre-check
    /// before paying a call for an unregistered bucket).
    fn markdown_transformer_count(&self) -> usize {
        0
    }

    /// Run the native tool resolvers for one call: tool-specific resolvers in
    /// registration order, then the fallback resolvers. Unhandled delegates to
    /// the JS chain / built-in rendering.
    fn render_tool(&self, _request: &ToolRenderRequest<'_>) -> RendererOutcome {
        RendererOutcome::default()
    }

    /// [`RendererService::render_tool`] with the upstream `renderShell` choice
    /// the handling registration (or the tool's own JS renderers) declared.
    /// The default wraps [`RendererService::render_tool`] with no shell
    /// metadata, so existing services keep compiling.
    fn render_tool_with_shell(&self, request: &ToolRenderRequest<'_>) -> RendererCallOutcome {
        RendererCallOutcome::from_outcome(self.render_tool(request))
    }
}

/// Completion registration and the unified suggestion entry point.
pub trait CompletionService: Send + Sync {
    /// `registerCommand(...).getArgumentCompletions`.
    fn register_argument(&self, _command: &str, _provider: Arc<dyn ArgumentCompletions>) {}

    /// `ctx.ui.addAutocompleteProvider`.
    fn register_provider(&self, _provider: Arc<dyn AutocompleteProvider>) {}

    /// Suggestions across every native provider (the same prefix-dedupe and
    /// merge the TUI applies to the JS chain).
    fn suggest(
        &self,
        _context: &CompletionContext,
    ) -> ServiceFuture<'static, Vec<AutocompleteItem>> {
        Box::pin(async { Vec::new() })
    }

    /// Whether a native argument-completion provider is registered for
    /// `command` (the TUI's cheap gate before dispatching a completion).
    fn has_argument_provider(&self, _command: &str) -> bool {
        false
    }

    /// Argument completions for one slash command; empty when no native
    /// provider is registered. The TUI merges these AFTER the JS
    /// `getArgumentCompletions` result.
    fn argument_completions(
        &self,
        _command: &str,
        _prefix: &str,
    ) -> ServiceFuture<'static, Vec<AutocompleteItem>> {
        Box::pin(async { Vec::new() })
    }

    /// Whether any global native autocomplete provider is registered (the
    /// TUI's cheap gate; mirrors `has_autocomplete_suggesters` for JS).
    fn has_providers(&self) -> bool {
        false
    }
}

/// Callback type of an [`EventService`] subscription.
pub type EventCallback = Arc<dyn Fn(&JsonValue) + Send + Sync>;

/// The cross-face extension *bus* (`pi.events`), NOT the plugin event
/// dispatch.
///
/// `emit` is synchronous (the JS bus is synchronous next to the host call);
/// callbacks run on the emitting thread. Cross-face JS↔Rust delivery is the
/// host's concern and may be asynchronous (audit risk R7).
pub trait EventService: Send + Sync {
    /// Emit one bus event (synchronous dispatch).
    fn emit(&self, _channel: &str, _data: JsonValue) {}

    /// Subscribe to one channel; cancel the returned token to unsubscribe.
    fn subscribe(&self, _channel: &str, _callback: EventCallback) -> Subscription {
        Subscription::noop()
    }
}

/// Native status-line cells (`ctx.ui.setStatus` / the Rust `statuses()`
/// equivalent).
///
/// Writes are cell updates, not action-queue entries: one bounded map write
/// and (at most) one repaint-signal bump. Hosts merge these with the JS
/// statuses using native-wins precedence (audit risk R6).
pub trait StatusService: Send + Sync {
    /// Set (or clear, with `None`) one status cell.
    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    /// Snapshot of all cells in insertion order.
    fn statuses(&self) -> StatusSnapshot {
        Arc::from(Vec::new())
    }

    /// One cell's current text.
    fn status(&self, key: &str) -> Option<Arc<str>> {
        self.statuses()
            .iter()
            .find(|(cell, _)| cell.as_ref() == key)
            .map(|(_, text)| Arc::clone(text))
    }
}

/// CLI flag values (`pi.getFlag`).
pub trait FlagService: Send + Sync {
    /// The registered flag's definition (default value).
    fn get(&self, _name: &str) -> Option<FlagValue> {
        None
    }

    /// The effective value: CLI value over the default.
    fn value(&self, name: &str) -> Option<FlagValue> {
        self.get(name)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for a [`PluginContext`] on the host side.
///
/// Hosts construct the service objects (each implementing one trait), attach
/// them here, and hand the result to
/// [`PluginHost::bind_context`](crate::plugins::PluginHost::bind_context).
/// Every service is optional; absent services fail closed through the trait
/// defaults and the context accessors return `None`.
///
/// The builder also owns the initial generation cell (value 0). A context
/// built here is usable immediately for direct/embedded callers, but stale
/// detection only becomes meaningful once a host has bound it through
/// `bind_context` (which installs the host's own cell).
pub struct PluginServices {
    pub(crate) session: Option<Arc<dyn SessionService>>,
    pub(crate) messages: Option<Arc<dyn MessageService>>,
    pub(crate) tools: Option<Arc<dyn ToolService>>,
    pub(crate) ui: Option<Arc<dyn UiService>>,
    pub(crate) providers: Option<Arc<dyn ProviderService>>,
    pub(crate) renderers: Option<Arc<dyn RendererService>>,
    pub(crate) completions: Option<Arc<dyn CompletionService>>,
    pub(crate) events: Option<Arc<dyn EventService>>,
    pub(crate) status: Option<Arc<dyn StatusService>>,
    pub(crate) flags: Option<Arc<dyn FlagService>>,
    pub(crate) identity: PluginIdentity,
    pub(crate) epoch_cell: Arc<std::sync::atomic::AtomicU64>,
}

impl PluginServices {
    /// An empty service set (every capability absent).
    pub fn new() -> Self {
        Self {
            session: None,
            messages: None,
            tools: None,
            ui: None,
            providers: None,
            renderers: None,
            completions: None,
            events: None,
            status: None,
            flags: None,
            identity: PluginIdentity::host(),
            epoch_cell: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Bind a session service.
    pub fn with_session(mut self, service: Arc<dyn SessionService>) -> Self {
        self.session = Some(service);
        self
    }

    /// Bind a message service.
    pub fn with_messages(mut self, service: Arc<dyn MessageService>) -> Self {
        self.messages = Some(service);
        self
    }

    /// Bind a tool service.
    pub fn with_tools(mut self, service: Arc<dyn ToolService>) -> Self {
        self.tools = Some(service);
        self
    }

    /// Bind a UI service.
    pub fn with_ui(mut self, service: Arc<dyn UiService>) -> Self {
        self.ui = Some(service);
        self
    }

    /// Bind a provider service.
    pub fn with_providers(mut self, service: Arc<dyn ProviderService>) -> Self {
        self.providers = Some(service);
        self
    }

    /// Bind a renderer service.
    pub fn with_renderers(mut self, service: Arc<dyn RendererService>) -> Self {
        self.renderers = Some(service);
        self
    }

    /// Bind a completion service.
    pub fn with_completions(mut self, service: Arc<dyn CompletionService>) -> Self {
        self.completions = Some(service);
        self
    }

    /// Bind the extension event bus.
    pub fn with_events(mut self, service: Arc<dyn EventService>) -> Self {
        self.events = Some(service);
        self
    }

    /// Bind the status-cell service.
    pub fn with_status(mut self, service: Arc<dyn StatusService>) -> Self {
        self.status = Some(service);
        self
    }

    /// Bind the flag service.
    pub fn with_flags(mut self, service: Arc<dyn FlagService>) -> Self {
        self.flags = Some(service);
        self
    }

    /// Set the identity used for a context not bound through a host (guides,
    /// diagnostics). `bind_context` overrides it per plugin.
    pub fn with_identity(mut self, identity: PluginIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Finish building. The context is standalone until a host binds it
    /// through `bind_context`; binding installs the host generation cell and
    /// enables stale detection.
    pub fn build(self) -> PluginContext {
        PluginContext::from(self)
    }
}

impl Default for PluginServices {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PluginServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginServices")
            .field("session", &self.session.is_some())
            .field("messages", &self.messages.is_some())
            .field("tools", &self.tools.is_some())
            .field("ui", &self.ui.is_some())
            .field("providers", &self.providers.is_some())
            .field("renderers", &self.renderers.is_some())
            .field("completions", &self.completions.is_some())
            .field("events", &self.events.is_some())
            .field("status", &self.status.is_some())
            .field("flags", &self.flags.is_some())
            .field("identity", &self.identity)
            .finish()
    }
}
