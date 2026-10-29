//! Headless agent session facade: provider registry + tools + session
//! storage + compaction + the agent loop, combined into one embeddable
//! API (the equivalent of upstream `AgentSession`).
//!
//! The facade never depends on the TUI and never reads `~/.pi` configuration
//! itself: the caller (an embedder, the RPC host, or a test) supplies the
//! [`ProviderRegistry`], [`Model`], tool registry, and optional
//! [`SessionManager`].  Each [`AgentSession::prompt`] run assembles the
//! context from session storage (or an in-memory transcript for ephemeral
//! sessions), drives [`crate::agent::agent_loop::run_with_options`] with the
//! steering/follow-up inboxes and the compaction hook, and appends the
//! produced assistant and tool-result messages back to session storage.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, Notify};

use crate::agent::agent_loop::{
    self, AgentEvent, CompactionFn, EventStreamFn, FinishTurnAction, FinishTurnContext,
    FinishTurnHook, FollowUpFn, LoopDeps, LoopOptions, PrepareRequestHook, ProviderRequest,
};
use crate::agent::tool_execution::ToolRegistry;
use crate::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::ProviderRegistry;
use crate::ai::providers::{DynamicProviderConfig, RegisterDynamicProviderShared};
use crate::ai::types::{
    AbortSignal, AssistantMessage, ContentBlock, Context, Message, Model, StopReason,
    StreamOptions, TextContent, ThinkingLevel, UserMessage, UserRole,
};
use crate::compaction::{self, CompactionConfig, CompactionResult, CompleteFn, ProjectedEntry};
use crate::config::settings::{FollowUpMode, SettingsManager, SteeringMode};
use crate::extensions::{ExtensionConfig, ExtensionHandle, Notification, SessionContextSnapshot};
use crate::plugins::PluginHost;
use crate::session_manager::projection::custom_message_text;
use crate::session_manager::{FileEntry, SessionManager};

/// Event listener for [`AgentEvent`] values (re-exported from the loop).
pub type EventHandler = agent_loop::EventHandler;
// Crate-visible: print mode (coding_agent/print.rs) builds the same registry /
// delivery / follow-up channel this module installs on an `AgentSession` and
// owns its own instance for the one-shot host (`PrintBackground`).
pub(crate) mod background;
mod bash;
mod build;
mod cache_warm;
mod compact;
mod diagnostics;
mod events;
#[cfg(feature = "extensions")]
pub mod ext_session_ops;
mod inbox;
mod mcp_reach;
mod models;
/// Native extension host interface: typed/cancellable session replacement and
/// floating custom overlays (`NativeSessionHost`).
pub mod native_host;
pub(crate) use models::scoped_cycle_target;
mod notifications;
pub mod phone_child;
mod prompt_input;
mod prompt_loop;
mod providers;
mod queue;
mod runtime_loader;
mod session_commands;
mod session_ctx;
mod session_store;
mod settle;
#[doc(hidden)]
pub use settle::PersistLedger;
mod stats;
mod stream;
mod tool_repair;
mod tools;
mod transcript;

// Crate-visible print-host seams: the sole consumer is `coding_agent/print.rs`
// (print mode drives the agent loop without an `AgentSession`). Gated on the
// `print` feature so a minimal SDK embed does not import them as dead code.
#[cfg(feature = "print")]
pub(crate) use notifications::drain_extension_notifications;
#[cfg(feature = "print")]
pub(crate) use prompt_input::transform_prompt_input;
#[cfg(feature = "print")]
pub(crate) use stream::provider_stream_event_hook;
#[cfg(feature = "print")]
pub(crate) use tool_repair::host_tool_repair_hook;
#[cfg(feature = "print")]
pub(crate) use tools::extension_tool_update_channel;

pub use background::BackgroundCallStatus;
#[doc(hidden)]
pub use background::FollowUpQueueProbe;
pub use bash::BashResult;
#[doc(hidden)]
pub use build::plugin_services::wait_for_claim_change_for_tests;
pub use events::{
    agent_event_to_json, registry_event_stream, session_event_to_json, SessionEvent,
    SessionEventHandler, SummarizationRetrySource,
};
pub use models::{resolve_starting_thinking_level, ModelCycle};
pub use native_host::{
    native_session_host, NativeCapability, NativeHostError, NativeHostFuture, NativeOverlayFactory,
    NativeSessionAction, NativeSessionHost, SessionReplacement,
};
pub use notifications::drain_extension_notifications_pub;
#[doc(hidden)]
pub use prompt_loop::trim_ephemeral_history;
pub use providers::serialize_scoped_models;
pub use queue::ClearedQueue;
#[cfg(feature = "subagent")]
pub(crate) use runtime_loader::partition_cached_extension_failures;
pub use runtime_loader::{
    apply_cli_extension_flags, dispatch_project_trust, dispatch_user_bash,
    dispatch_user_bash_with_host, load_extension_runtime, load_extension_runtime_strict,
    load_extension_runtime_strict_with_catalog, load_extension_runtime_with_catalog,
    stash_validated_extension_runtime, CliExtensionFlagDiagnostic, ModelCatalog,
};
pub use session_commands::{
    apply_session_command_to_storage, commit_navigation_to_storage, generate_navigation_summary,
    prepare_session_command_to_storage, record_session_command_action,
    take_session_command_actions, take_session_command_actions_for, NavigateSummaryBinding,
    NavigateSummaryOutcome, PreparedNavigateTree, SessionCommandAction, SessionCommandActionEntry,
    SessionCommandStep, SessionOrigin, SessionReloadOutcome,
};
pub use session_store::{ForkEntryOutcome, ForkPosition};
pub use stats::{compute_cache_waste, usage_cost_breakdown, CacheWasteTotals, UsageCostEntry};

// Internal cross-submodule helpers: re-exported (privately) so every child
// module of this facade reaches them through `use super::*`.
use bash::PendingBashRecord;
pub(crate) use compact::{compaction_details, compaction_result_wire};
use events::{emit_session_event_to, CompactionRunGuard};
use inbox::Inbox;
use models::parse_thinking_level;
use notifications::{content_text, emit_plugin_event, report_plugin_diagnostic};
use prompt_input::{transform_prompt_input_host, PromptInputTransform};
use providers::restore_session_virtual_models;
// Hosts that resolve a resumed session's recorded model BEFORE the constructor
// runs (the SDK builder, `Agent::attach_session`, the server's lazy rebuild)
// restore the branch's virtual registrations through this seam, then use the
// returned declared keys to tell an unresolvable virtual id from a stale
// physical one. Only those two hosts need it, so the bare core omits it.
#[cfg(any(feature = "sdk", feature = "server"))]
pub(crate) use providers::restore_virtual_models_from;
use queue::{follow_up_fn, steering_fn};
use runtime_loader::load_settings_for;
// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`AgentSession::new`].
pub struct AgentSessionConfig {
    /// Provider registry used for streaming and (compaction) completion calls.
    pub provider_registry: Arc<ProviderRegistry>,
    /// The model to drive. The facade never consults `~/.pi` configuration.
    pub model: Model,
    /// Explicit API key override; `None` lets keyless providers work as-is.
    ///
    /// The key is scoped to THIS model's provider (`model.provider`): the
    /// session never sends it to a provider selected later by
    /// [`AgentSession::set_model`], which resolves that provider's own
    /// credential instead (upstream `--api-key` / models.json `apiKey`
    /// semantics).
    pub api_key: Option<String>,
    /// Requested thinking level (emitted as reasoning effort where supported).
    pub thinking: ThinkingLevel,
    /// Tools available to the model.
    pub tools: ToolRegistry,
    /// Session storage; `None` keeps the transcript in memory only.
    pub session: Option<SessionManager>,
    /// Automatic compaction settings; `None` disables compaction.
    pub compaction: Option<CompactionConfig>,
    /// Maximum number of loop turns per prompt.
    pub max_turns: usize,
    /// Optional system prompt prepended to every request.
    pub system_prompt: Option<String>,
    /// Working directory supplied to tool executions (default: process cwd).
    pub cwd: Option<PathBuf>,
    /// Optional stream override (embedder/test hook); when set it replaces the
    /// registry-based stream and supplies full stream lifecycle events.
    pub event_stream: Option<SharedEventStreamFn>,
    /// Optional JS extension runtime. When present, its tools are merged into
    /// the registry (including tools registered by `session_start`) and the
    /// `session_start`, `input`, `tool_call`, and `tool_result` extension
    /// events are dispatched around prompt runs.
    pub extensions: Option<crate::extensions::ExtensionHandle>,
    /// Resolved tool selection (`--tools` / `--exclude-tools` /
    /// `settings.defaultTools`, upstream settings-manager semantics; only `*`
    /// is a wildcard — `?` is literal and matching is case-sensitive, see
    /// `tool_pattern_matches`). When set it is the ACTIVE override (the F12
    /// rule): the registry is NOT filtered, so every registered tool stays
    /// registered and callable (codemode/deferred scripts, transcript-restored
    /// names), while `defaultActive: false` tools (e.g. `powershell`) activate
    /// exactly when the selection names them. MCP tools the allowlist keeps are
    /// already part of the resolved selection.
    pub tool_selection: Option<crate::coding_agent::ToolSelection>,
}

/// Sharable stream override: same shape as the loop's [`EventStreamFn`] but
/// wrapped in an `Arc` so a session can hand it to every prompt run. Receives
/// the loop's [`ProviderRequest`] (context plus the runtime model/thinking
/// level, both possibly replaced by a `prepare_request` hook).
pub type SharedEventStreamFn = Arc<
    dyn Fn(ProviderRequest) -> agent_loop::BoxFuture<'static, AssistantMessageEventStream>
        + Send
        + Sync,
>;

/// Projection snapshot handed to the compaction hook: the prompt-start
/// per-entry METADATA (source entry + contributed message count) and the leaf
/// path metadata. It carries no messages: the hook slices the live context it
/// is handed ([`compaction::ProjectionEntryMeta::cut_refs`]), so the snapshot
/// costs a handful of bytes per entry instead of a copy of the conversation.
pub(crate) type ProjectionSnapshot = (
    Arc<Vec<compaction::ProjectionEntryMeta>>,
    Arc<Vec<compaction::PathEntryMeta>>,
);

/// Write-on-replace snapshot of the ephemeral transcript (upstream keeps one
/// message array too): read paths clone the `Arc` (zero copy) and writers
/// copy-on-write via [`Arc::make_mut`] or replace the slot wholesale, so a
/// read never deep-copies the message vector.
type HistorySnapshot = Arc<Vec<Message>>;

/// Cached merged-registry entry: the plugin host's registry-definition
/// generation it was built from, the `setActiveTools` override in effect, the
/// cumulative materialization count (test/observability seam), and the built
/// registry.
///
/// The generation moves on every host registry rebuild, so a same-name
/// definition change (an MCP description/schema update, a JS re-registration)
/// invalidates the entry without cloning any tool definitions on the
/// cache-hit path.
type MergedRegistryCache = (u64, Option<Vec<String>>, u64, Arc<ToolRegistry>);

/// Snapshot-lite of the live session state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSnapshot {
    /// Model id currently in use.
    pub model: String,
    /// Provider id currently in use.
    pub provider: String,
    /// Requested thinking level.
    pub thinking: ThinkingLevel,
    /// Turns executed since the session was created.
    pub turns: usize,
    /// Total tokens reported by assistant usage records so far.
    pub tokens: u64,
    /// Whether a prompt run is currently in flight.
    pub streaming: bool,
    /// Session file id, when persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Session file path, when persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
}

/// How an input (prompt/steer/follow-up) was or would be disposed of by an
/// [`AgentSession`] (upstream `PromptDisposition` / `QueuedInputDisposition`,
/// agent-session.ts:310-311; serialized as the RPC `data.disposition` wire
/// string). Upstream has no `dropped` value: a rejected input (e.g. an
/// extension-command queue attempt) raises a plain error instead, so the
/// former `Dropped` variant is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Disposition {
    /// The input started (or would start) a prompt run — wire value
    /// `"started"` (upstream `prompt()` preflight, agent-session.ts:2109).
    Started,
    /// The input was consumed and will be delivered immediately (upstream
    /// `QueuedInputDisposition "handled"`): steer/follow-up while the agent
    /// is idle.
    Handled,
    /// The input was queued for delivery during the current or next run —
    /// wire value `"queued"` (upstream `_queueUserInput`).
    Queued,
}

/// Queue drain policy applied to the steering/follow-up queues (upstream
/// `QueueMode`); see [`AgentSession::steering_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueMode {
    /// Deliver every queued message on the next poll (upstream `"all"`).
    All,
    /// Deliver exactly one queued message per turn (upstream
    /// `"one-at-a-time"`, the default).
    OneAtATime,
}

/// The session's run/compaction claim: at most ONE owner at a time, taken by
/// a compare-exchange from [`SESSION_IDLE`] — the mutual exclusion itself, so
/// the busy state is exact (a run can never start mid-compaction, and a
/// manual compaction can never start mid-run). Runs release the claim through
/// their RAII guard ([`prompt_loop::SessionStreamingGuard`]) and manual
/// compactions through theirs (`compact::ManualCompactionClaim`); every
/// release wakes [`AgentSession::idle_notify`] waiters.
///
/// `IDLE`/`RUNNING` are crate-visible because the print host owns a private
/// claim flag of the same shape: `background::BackgroundDelivery` gates the
/// live follow-up injection on exactly `SESSION_RUNNING`, and print flips its
/// own flag around each prompt run (there is no compaction state to model).
pub(crate) const SESSION_IDLE: u8 = 0;
/// A prompt run owns the session (upstream `_isAgentRunActive`);
/// [`AgentSession::is_streaming`] reports exactly this state.
pub(crate) const SESSION_RUNNING: u8 = 1;
/// Test mirror of [`SESSION_IDLE`] for the event-driven claim-wait regression.
#[doc(hidden)]
pub const SESSION_IDLE_STATE_FOR_TESTS: u8 = SESSION_IDLE;
/// Test mirror of [`SESSION_RUNNING`] for the event-driven claim-wait regression.
#[doc(hidden)]
pub const SESSION_RUNNING_STATE_FOR_TESTS: u8 = SESSION_RUNNING;
/// A manual compaction owns the session (upstream's armed
/// `_compactionAbortController`, which makes `isCompacting` true and refuses
/// new prompts).
const SESSION_COMPACTING: u8 = 2;

/// Token/cost/message totals over the whole session (upstream `SessionStats`,
/// the `get_session_stats` RPC payload). Aggregated over every session entry,
/// including history that compaction removed, so the totals reflect what was
/// actually billed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    /// Session file path, when persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// Session id.
    pub session_id: String,
    /// User messages in the transcript.
    pub user_messages: usize,
    /// Assistant messages in the transcript.
    pub assistant_messages: usize,
    /// Tool calls issued by assistant messages.
    pub tool_calls: usize,
    /// Tool-result messages in the transcript.
    pub tool_results: usize,
    /// Every message entry in the transcript.
    pub total_messages: usize,
    /// Aggregated token usage.
    pub tokens: SessionTokens,
    /// Aggregated cost (USD) across the same usage records.
    pub cost: f64,
    /// Context-window usage, when the model reports a window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
}

/// Token totals of [`SessionStats`] (upstream `SessionStats.tokens`).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// Context-window usage estimate (upstream `ContextUsage`); `tokens`/`percent`
/// are `None` when the estimate is unknown.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

// ---------------------------------------------------------------------------
// AgentSession
// ---------------------------------------------------------------------------

/// Outcome of a successful tree navigation (upstream `navigateTree` result,
/// agent-session.ts:3966-3970).
#[derive(Debug, Clone, Default)]
pub struct TreeNavigation {
    /// Editor text when the target was a user message / custom message
    /// (upstream `editorText`): hosts prefill the input with it.
    pub editor_text: Option<String>,
    /// Id of the appended `branch_summary` entry, when one was stored
    /// (upstream `summaryEntry.id`).
    pub summary_entry_id: Option<String>,
    /// The branch summarization was cancelled mid-flight (upstream
    /// `{cancelled: true, aborted: true}`, agent-session.ts:4061-4063): the
    /// leaf and the transcript are UNCHANGED. Hosts show the upstream
    /// "Branch summarization cancelled" status and re-open the tree selector.
    pub aborted: bool,
    /// A `session_before_tree` handler cancelled the navigation before any
    /// summarization or leaf move (upstream `{cancelled: true}`,
    /// agent-session.ts:4055-4058): the leaf and the transcript are
    /// UNCHANGED, and no branch summary was requested from the model.
    pub cancelled: bool,
}

/// Transport-retry lifecycle notifications for hosts (upstream
/// `auto_retry_start` / `auto_retry_end` / `summarization_retry_*` session
/// events, agent-session.ts:220-230): the TUI renders the
/// `RetryStatusIndicator` countdown from `Scheduled` and clears it on
/// `Settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryEvent {
    /// A retry was scheduled (upstream `auto_retry_start` /
    /// `summarization_retry_scheduled`): show the countdown for `delay_ms`.
    Scheduled(crate::ai::types::RetryNotice),
    /// The wait finished and the request proceeds (upstream
    /// `auto_retry_end { success: true }` / `summarization_retry_attempt_start`).
    Settled,
    /// The wait was cancelled by the user (upstream `abortRetry()` →
    /// `auto_retry_end { success: false, finalError: "Retry cancelled" }`).
    Cancelled(crate::ai::types::RetryNotice),
}

/// Host sink for [`RetryEvent`]s (the TUI's countdown): installed by the
/// worker host, invoked from the transport's retry wait.
pub type RetryNoticeSink = Arc<dyn Fn(RetryEvent) + Send + Sync>;

/// One-shot cancellation latch for a running operation (upstream per-operation
/// `AbortController` fields: `_branchSummaryAbortController`,
/// `_retryAbortController`).
///
/// A host must be able to cancel an operation while the worker's command loop
/// is blocked inside it (single consumer), so the latch is shared out of band:
/// a clone of the handle fires the ACTIVE operation's signal. Each operation
/// arms a FRESH signal when it starts (upstream
/// `this._branchSummaryAbortController = new AbortController()`), so a stale
/// cancel from a settled operation can never cancel a later one.
#[derive(Clone, Default)]
pub struct CancelLatch {
    inner: Arc<std::sync::Mutex<Option<crate::ai::types::AbortSignal>>>,
}

/// The `/tree` branch-summarization latch (upstream
/// `_branchSummaryAbortController`).
pub type BranchSummaryAbort = CancelLatch;

impl CancelLatch {
    /// Arm a fresh signal for one summarization and return it (the signal
    /// clones share the token, so [`Self::cancel`] aborts it).
    pub fn arm(&self) -> crate::ai::types::AbortSignal {
        let signal = crate::ai::types::AbortSignal::new();
        *self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(signal.clone());
        signal
    }

    /// Abort the armed signal (upstream `abortBranchSummary()`); a no-op
    /// when nothing is armed.
    pub fn cancel(&self) {
        if let Some(signal) = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            signal.abort();
        }
    }

    /// Whether a signal is armed (test seam).
    pub fn is_armed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Whether the armed signal was aborted (test seam); `false` when
    /// nothing is armed.
    pub fn is_cancelled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(crate::ai::types::AbortSignal::is_aborted)
    }
}

/// The editor text of a stored message (upstream `extractUserMessageText` /
/// `contentText(targetEntry.content)`).
fn stored_message_text(message: &crate::session_manager::StoredMessage) -> String {
    match message {
        crate::session_manager::StoredMessage::User(user) => {
            crate::ai::api::joined_text_blocks(&user.content, "")
        }
        crate::session_manager::StoredMessage::Assistant(assistant) => {
            crate::ai::api::joined_text_blocks(&assistant.content, "")
        }
        crate::session_manager::StoredMessage::Custom(inner) => custom_message_text(&inner.content),
        _ => String::new(),
    }
}

/// Embeddable headless agent: provider + tools + storage + compaction + loop.
pub struct AgentSession {
    registry: Arc<ProviderRegistry>,
    /// Current model. Interior-mutable so `pi.setModel` (a notification
    /// applied at the end of a run) can switch it for subsequent runs.
    model: RwLock<Model>,
    /// API keys contributed by extension-registered providers
    /// (`pi.registerProvider`), applied to stream options when the current
    /// model belongs to that provider.
    extension_api_keys: Mutex<HashMap<String, String>>,
    /// The explicit API key captured at construction
    /// ([`AgentSessionConfig::api_key`]) together with the provider it was
    /// resolved for.
    ///
    /// It is PROVIDER-SCOPED on purpose: a `/model` switch to another provider
    /// must resolve that provider's own credential instead of sending this one
    /// (see `AgentSession::request_api_key_for`). `Debug` is hand-written for
    /// this reason.
    api_key: Option<stream::ProviderApiKey>,
    /// Current thinking level. Interior-mutable so `pi.setThinkingLevel` (a
    /// notification applied at the end of a run) can change it for subsequent
    /// runs.
    thinking: RwLock<ThinkingLevel>,
    tools: Arc<ToolRegistry>,
    /// The host-resolved tool selection (upstream `--tools` / settings
    /// `defaultTools`). When set it is also the ACTIVE override, so
    /// `defaultActive: false` tools (e.g. `powershell`) activate exactly when
    /// the selection names them.
    tool_selection: Option<crate::coding_agent::ToolSelection>,
    /// The active-tool override cell extensions write through (upstream
    /// `setActiveTools`): the `tool_search` builtin's matches land here and
    /// take effect on the next tool-loadout rebuild. Consulted FIRST in the
    /// loadout precedence chain, before the plugin-host override and the
    /// host-resolved `tool_selection`.
    active_tools_override: Arc<RwLock<Option<Vec<String>>>>,
    /// Host-registered live partial-result sink (the TUI's tool-block
    /// streaming): receives the call-id-carrying payloads the scheduler
    /// wraps, independent of any extension runtime.
    host_tool_updates: Arc<Mutex<Option<crate::agent::tool_execution::ToolUpdateFn>>>,
    scheduler: ToolScheduler,
    /// In-flight background tool calls (`background: true`): ownership,
    /// limit, and the abort handles of the detached futures
    /// (`TOOL-BACKGROUND-SCOPE.md` §2.3). The scheduler's detach sink is
    /// installed over this registry at construction, so every execution path
    /// of the session — model batches and nested calls alike — shares it.
    background: Arc<background::BackgroundRegistry>,
    session: Option<Arc<tokio::sync::Mutex<SessionManager>>>,
    /// Cached session identity; interior-mutable so the new-session/fork/
    /// clone seams ([`AgentSession::replace_session_storage`]) can refresh it.
    session_id: RwLock<Option<String>>,
    session_path: RwLock<Option<PathBuf>>,
    /// Monotone storage generation, bumped by every storage swap
    /// ([`AgentSession::replace_session_storage`]). Long-running work that was
    /// started against one transcript (user `!` bash) captures it and refuses
    /// to persist its result once the slot names a different session.
    session_storage_generation: AtomicU64,
    /// Session title for sessions without storage (upstream keeps the name in
    /// the session manager; ephemeral sessions hold it here).
    session_name: RwLock<Option<String>>,
    /// In-memory transcript used when no session storage is configured, as a
    /// write-on-replace shared snapshot ([`HistorySnapshot`]).
    history: Arc<RwLock<HistorySnapshot>>,
    /// Automatic compaction settings; `None` disables compaction entirely.
    /// Interior-mutable (and shared with the run's compaction hook) so
    /// `set_auto_compaction` can flip it at any time (upstream
    /// `setAutoCompactionEnabled`).
    compaction: Arc<RwLock<Option<CompactionConfig>>>,
    max_turns: usize,
    cwd: PathBuf,
    /// The structured system prompt. `RwLock` so the `/reload` hot-refresh
    /// seam ([`AgentSession::set_prompt_resources_shared`]) can rebuild the
    /// context-derived prompt's skills section in the running session.
    system_prompt: std::sync::RwLock<Option<String>>,
    /// The discovery inputs and `(declared, hidden)` snapshot the
    /// context-derived system prompt was last built from. A runtime
    /// active-set change (`setActiveTools`, `tool_search`, MCP discovery)
    /// rebuilds the prompt from these cached inputs without repeating
    /// context-file/skill discovery (D6; upstream re-runs
    /// `_rebuildSystemPrompt` on every active-set change). `None` when the
    /// prompt is caller-supplied or the builder found nothing to inject.
    prompt_context: std::sync::RwLock<Option<build::PromptContextCache>>,
    /// `PI_PACKAGE_DIR` resolved ONCE at build (upstream `getPackageDir`):
    /// the session build, `/reload` and active-set prompt rebuilds all use
    /// this value, so a mid-process change cannot split the prompt across two
    /// installations (D1's pin applied to the prompt's `docs` section).
    package_dir: Option<PathBuf>,
    event_stream: Option<SharedEventStreamFn>,
    extensions: Option<ExtensionHandle>,
    /// The unified plugin host wrapping the JS runtime plus every enabled
    /// Rust builtin plugin: ONE ordered dispatch list, ONE shared registry
    /// (`src/plugins/`). All session-level event dispatch walks it.
    plugins: Option<Arc<PluginHost>>,
    /// Whether `session_start` has already been dispatched to the extension
    /// runtime (fired once, before the first prompt).
    session_start_sent: AtomicBool,
    /// Whether the "MCP tools are unreachable" warning has been printed
    /// (upstream `warnedUnreachable`, one per session; see
    /// [`AgentSession::ensure_discovery_active`]). Without the `mcp` feature
    /// no MCP server can be configured, so the flag is absent.
    #[cfg(feature = "mcp")]
    mcp_unreachable_warned: AtomicBool,
    /// Cached `base tools + plugin tools` registry, keyed by the sorted
    /// tool-name snapshot it was built from plus the `setActiveTools`
    /// override in effect. Rebuilt only when either changes (`session_start`
    /// / command registrations / setActiveTools), so prompt runs reuse one
    /// registry.
    merged_extension_tools: Mutex<Option<MergedRegistryCache>>,
    /// Queued steering messages: ONE command channel served by ONE consumer
    /// task ([`Inbox`]; the run polls per [`Self::steering_mode`], read at
    /// each poll, so `clear_queue` can drop messages mid-run).
    steering_inbox: Inbox,
    /// Queued follow-up messages (see [`Self::steering_inbox`]).
    follow_up_inbox: Inbox,
    /// Queued background tool-result frames. Kept apart from the user
    /// follow-up queue: the loop delivers these at tool-batch boundaries so a
    /// long tool chain still receives completions, while ordinary user
    /// follow-ups wait for the end of the inner loop (upstream semantics).
    background_follow_up_inbox: Inbox,
    /// Steering queue drain mode (upstream `steeringMode`, default
    /// `one-at-a-time`).
    steering_mode: Arc<RwLock<SteeringMode>>,
    /// Follow-up queue drain mode (upstream `followUpMode`, default
    /// `one-at-a-time`).
    follow_up_mode: Arc<RwLock<FollowUpMode>>,
    /// Active bash-execution abort signals (upstream `_bashAbortControllers`,
    /// a `Set<AbortController>`): every `execute_bash` registers its own
    /// signal and removes it once its command settles, so concurrent
    /// commands (RPC `bash` and extension-triggered bash) can all be
    /// cancelled. Entries are paired with a generation id so removal never
    /// drops a concurrent sibling's signal.
    bash_abort: Mutex<Vec<(u64, AbortSignal)>>,
    /// Generation counter backing [`Self::bash_abort`] entry identities.
    bash_abort_seq: AtomicU64,
    /// Bash-execution records deferred while a run is streaming (upstream
    /// `_pendingBashMessages`); flushed on settle.
    pending_bash: Mutex<Vec<PendingBashRecord>>,
    /// Whether the transport retries failed requests (upstream
    /// `autoRetryEnabled` / the `retry.enabled` setting, default true).
    /// `set_auto_retry_enabled` flips it; `abort_retry` cancels only the
    /// pending wait and leaves this untouched (upstream `abortRetry`).
    auto_retry: AtomicBool,
    /// The `/tree` branch-summarization cancellation latch (upstream
    /// `_branchSummaryAbortController`): `navigate_tree` arms it per
    /// navigation, hosts cancel through a shared clone while the worker
    /// command loop is blocked inside the summary.
    branch_summary_abort: BranchSummaryAbort,
    /// The transport-retry cancellation latch (upstream
    /// `_retryAbortController`): the retry wait arms it per attempt and
    /// `abort_retry()` fires it, so Esc during the countdown cancels the wait
    /// (upstream `abortRetry`).
    retry_cancel: CancelLatch,
    /// Host-installed retry-notice sink (the TUI countdown); `None` keeps the
    /// transport's own sleep and no countdown is rendered.
    host_retry_notice: Mutex<Option<RetryNoticeSink>>,
    signal: Mutex<AbortSignal>,
    turns: AtomicUsize,
    tokens: AtomicU64,
    /// The session's run/compaction claim ([`SESSION_IDLE`] /
    /// [`SESSION_RUNNING`] / [`SESSION_COMPACTING`]): one CAS-claimed owner
    /// at a time, so the flag transitions ARE the mutual exclusion (see the
    /// constants above). `Arc` so the run's guard and the loop's compaction
    /// hook can release/flip it.
    session_state: Arc<AtomicU8>,
    /// Whether the in-flight run has been asked to abort (upstream
    /// `_agentRunAbortRequested`): covers the window between `abort()` and
    /// the run's future observing its signal, and lets a manual `compact()`
    /// tell a genuinely streaming run (reject with the honest error) from one
    /// already on its way out (wait for it, upstream `compact()` opens with
    /// `await this.abort()`).
    run_abort_requested: Arc<AtomicBool>,
    /// Set once the in-flight run's agent loop reached its terminal result
    /// (no further model request will be issued; only post-run settlement
    /// work remains): a manual `compact()` arriving in that window waits for
    /// the run's claim to release instead of failing, matching upstream's
    /// idle boundary (`_emitAgentSettled` clears `_isAgentRunActive` before
    /// the settle event).
    run_settling: Arc<AtomicBool>,
    /// Woken whenever the session claim returns to idle. A waiter registers
    /// with `Notify` BEFORE re-checking the claim, so a release between the
    /// check and the await is never missed (no busy-wait, no sleep).
    idle_notify: Arc<Notify>,
    /// Custom messages sent with `triggerTurn: true` while idle (upstream
    /// `sendCustomMessage` case 3): each one STARTS a run whose starting
    /// message it is. The facade has no idle loop of its own, so the run is
    /// started by the host draining this queue
    /// ([`AgentSession::take_pending_custom_run`] →
    /// [`AgentSession::run_custom_turn`]).
    pending_custom_runs: Arc<Mutex<VecDeque<CustomRunMessage>>>,
    /// Custom messages sent with `deliverAs: "nextTurn"` (upstream
    /// `_pendingNextTurnMessages`): they ride the next run's starting message
    /// and are consumed by it.
    pending_next_turn_messages: Arc<Mutex<Vec<CustomRunMessage>>>,
    /// Custom messages appended with `triggerTurn: false` on a store-less
    /// (ephemeral) session: upstream `_appendCustomMessage` always has a
    /// session manager to append to, so pir holds them in memory instead of
    /// dropping them and the next run drains them into its first
    /// request/history (see `consume_send_message`).
    pending_setup_messages: Arc<Mutex<Vec<CustomRunMessage>>>,
    /// Queued-but-not-yet-delivered steering/follow-up messages (backs
    /// `ctx.hasPendingMessages()`).
    pending_messages: Arc<AtomicUsize>,
    /// Host-installed `prepare_request` hook (upstream `config.prepareRequest`),
    /// applied to every prompt run until replaced.
    prepare_request: RwLock<Option<PrepareRequestHook>>,
    /// Host-installed `finish_turn` hook (upstream `config.finishTurn`),
    /// applied to every prompt run until replaced.
    finish_turn: RwLock<Option<FinishTurnHook>>,
    /// Host-installed per-tool lifecycle hooks (upstream
    /// `AgentLoopConfig.beforeToolCall` / `afterToolCall`), applied to every
    /// prompt run until replaced. `Arc` so the run hands the one handle to the
    /// scheduler and the nested-call executor without cloning hook closures.
    tool_hooks: RwLock<Option<Arc<crate::agent::tool_execution::ToolHooks>>>,
    /// Whether inline Rust plugins were attached at construction
    /// ([`AgentSession::new_with_plugins`]); gates the plugin-host tool
    /// update dispatch.
    has_inline_plugins: bool,
    /// Per-run stream option overrides (retry tuning for embedders), applied
    /// on top of the base options of every request.
    stream_overrides: RwLock<StreamRetryOverride>,
    /// Cache warming (upstream `cacheWarming`): `None` keeps it off (the
    /// default for direct SDK sessions); hosts install the resolved settings
    /// via [`AgentSession::set_cache_warming`].
    cache_warming: RwLock<Option<crate::cache_warmer::CacheWarmingSettings>>,
    /// The live cache-warm request's [`AbortSignal`]; aborted by the next
    /// prompt (the cached prefix is about to change). `None` while no warm
    /// is in flight — a fresh warm replaces the stale entry.
    warm_abort: Mutex<Option<AbortSignal>>,
    /// Discovered skills backing `/skill:<name>` expansion (upstream
    /// `resourceLoader.getSkills()`); empty unless the host installs them via
    /// [`AgentSession::set_prompt_resources`]. `RwLock` so `/reload` can
    /// hot-refresh them into the RUNNING session
    /// ([`AgentSession::set_prompt_resources_shared`], upstream
    /// `session.reload` re-discovery).
    skills: std::sync::RwLock<Vec<crate::skills::SkillInfo>>,
    /// Discovered prompt templates backing `/template` expansion (upstream
    /// `promptTemplates`); empty unless the host installs them via
    /// [`AgentSession::set_prompt_resources`]. `RwLock` for the same
    /// `/reload` hot-refresh seam.
    prompt_templates: std::sync::RwLock<Vec<crate::coding_agent::prompt_templates::PromptTemplate>>,
    /// The resources the LAST `resources_discover` dispatch contributed
    /// (name + source path). A reload drops its own previous contribution
    /// before merging the fresh one, so a changed, renamed, or removed
    /// contributed skill/template takes effect instead of being shadowed by
    /// the first-wins merge (see
    /// [`AgentSession::reload_extension_resources`]).
    extension_resource_contribution: std::sync::RwLock<session_ctx::ExtensionResourceContribution>,
    /// True when the system prompt was left to the context builder
    /// (`system_prompt: None`), so installing resources may rebuild its
    /// `skills` section.
    context_derived_prompt: bool,
    /// Session-only model scope for cycling (upstream `_scopedModels`, from
    /// the `--models` flag or `/scoped-models` toggles). Empty = no scope;
    /// [`AgentSession::cycle_model`] then cycles every available model.
    scoped_models: RwLock<Vec<crate::ai::providers::ScopedModel>>,
    /// Session-level event listeners (upstream `session.subscribe`):
    /// `queue_update` / `compaction_start` / `compaction_end` /
    /// `bash_execution_update` flow here — the events the agent-loop
    /// [`AgentEvent`] enum cannot carry (the loop event stream is unchanged).
    session_events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
    /// Whether a compaction run is in flight (upstream `isCompacting`):
    /// set around EVERY compaction run (manual / threshold / overflow).
    /// `Arc` so the loop's compaction hook (a `'static` closure) can flip it.
    compacting: Arc<AtomicBool>,
    /// The in-flight AUTOMATIC (threshold) compaction's own cancel slot:
    /// `(generation, signal)`, armed when the trigger fires and cleared when
    /// that compaction exits. [`AgentSession::abort_compaction`] fires it
    /// WITHOUT touching the run signal, so cancelling an automatic compaction
    /// neither aborts the turn nor leaks into the next one (every trigger
    /// arms a fresh signal).
    auto_compaction_cancel: Arc<Mutex<Option<(u64, AbortSignal)>>>,
    /// Weak self handle filled by the `Arc` constructors. Lets a `&self`
    /// operation (a storage swap, a reload) re-register the live session in
    /// the process-wide native host registry without changing every caller to
    /// hold the `Arc`.
    self_weak: RwLock<Option<std::sync::Weak<AgentSession>>>,
    /// Mirror of the queued steering/follow-up texts (upstream
    /// `_steeringMessages` / `_followUpMessages`), backing the
    /// `queue_update` event snapshots and kept in sync with the
    /// [`Inbox`] consumer's actual queue (every enqueue/drain site updates
    /// both sides).
    queue_mirror: Arc<Mutex<QueueMirror>>,
    /// The most recently finished run's diagnostics summary (per-turn
    /// provider wait / stream / tool / token breakdown assembled from the
    /// loop's event stream, `crate::diagnostics`). Collected on every run —
    /// counter-only work — so hosts can render it (e.g. a `/diagnostics`
    /// command) without an env gate; read with
    /// [`AgentSession::last_run_diagnostics`].
    last_run_diagnostics: std::sync::RwLock<Option<crate::diagnostics::RunDiagnostics>>,
    /// Fingerprint of the last `ctx.sessionManager` snapshot pushed into the
    /// extension/plugin host (see `agent_session/session_ctx.rs`). The push
    /// runs before and after every prompt; when the session entries, leaf,
    /// model, and the scalar snapshot cells are unchanged, the runtime already
    /// holds the identical snapshot and the expensive session-derived payload
    /// is not rebuilt.
    session_context_cache: Mutex<Option<session_ctx::SessionContextCache>>,
    /// Whether this process was spawned as a parallel phone session
    /// (`PI_PHONE_CHILD=1`). Read ONCE at construction so the per-prompt
    /// guard is a bool test; parent sessions are byte-for-byte unchanged.
    /// Only the RPC host reads it (the inline phone prompt planner), so a
    /// minimal SDK embed does not carry the dead field.
    #[cfg(feature = "rpc")]
    phone_child: bool,
    /// Bounded queue of native plugin actions (`MessageService`) awaiting the
    /// next drain. A native service call queues the SAME JSON payload the JS
    /// host function produces; [`AgentSession::flush_native_actions`] applies
    /// it through the same `consume_*` handlers the JS notification drain
    /// uses, so both faces persist identical shapes. Capacity is
    /// [`NATIVE_ACTION_QUEUE_CAP`].
    native_actions: Arc<Mutex<VecDeque<(NativeActionKind, JsonValue)>>>,
    /// Serializes `flush_native_actions`: a service call may flush from a
    /// detached runtime task, and two flushes draining different batches must
    /// not apply them out of enqueue order.
    native_action_flush: tokio::sync::Mutex<()>,
    /// The agent config directory this session identifies with (upstream
    /// `getAgentDir`), resolved exactly ONCE at build: the explicit
    /// [`AgentSession::new_with_agent_dir`] override when the host supplied
    /// one, else [`crate::ai::providers::agent_dir`] read from the process
    /// environment at that moment.
    ///
    /// Every session-scoped read of `auth.json`, the merged settings, and the
    /// sessions root goes through [`Self::agent_dir`], so a mid-process
    /// `PIR_AGENT_DIR` change cannot split one live session across two
    /// directories (D1). A session built afterwards still follows the new
    /// environment, exactly like a fresh process.
    agent_dir: PathBuf,
}

impl AgentSession {
    /// The agent directory this session is bound to, resolved at build time
    /// (see the field documentation). This is the session's identity for
    /// every session-scoped agent-dir read.
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// `<agent-dir>/sessions` for THIS session's agent directory — the root
    /// session-discovery listings are scoped to (see
    /// [`crate::session_manager::sessions_dir_in`]).
    pub fn sessions_dir(&self) -> PathBuf {
        crate::session_manager::sessions_dir_in(&self.agent_dir)
    }

    /// The merged settings snapshot for this session: the global scope of the
    /// session's agent directory overlaid by `<cwd>/.pi/settings.json` when
    /// the project is trusted (the same load the build performs, see
    /// `runtime_loader::load_settings_for_in`).
    ///
    /// Read on every call (settings edits stay visible to a live session),
    /// but always from the SESSION'S directory, never from the live
    /// environment.
    pub fn session_settings(&self) -> SettingsManager {
        runtime_loader::load_settings_for_in(&self.agent_dir, &self.cwd)
    }

    /// Credential storage scoped to this session's agent directory
    /// (`<agent-dir>/auth.json`). Hosts that authenticate or refresh inside a
    /// live session must use this instead of
    /// [`crate::ai::auth::FileAuthStorage::default_storage`], which
    /// re-resolves the process environment on every call.
    pub fn auth_storage(&self) -> crate::ai::auth::FileAuthStorage {
        crate::ai::auth::FileAuthStorage::in_agent_dir(&self.agent_dir)
    }
}

/// Which session action a queued native-service payload performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeActionKind {
    /// `pi.sendMessage(message, options)`.
    SendMessage,
    /// `pi.sendUserMessage(content, options)`.
    SendUserMessage,
    /// `pi.appendEntry(customType, data)`.
    AppendEntry,
}

/// Upper bound on queued native plugin actions. A pathological producer drops
/// the OLDEST action instead of growing memory without limit — the same
/// policy the runtime's notification queue applies to overflowing updates.
/// Actions are small (one message payload), so the bound costs nothing on the
/// normal path.
pub(crate) const NATIVE_ACTION_QUEUE_CAP: usize = 64;

/// Mirrored pending texts of the steering/follow-up queues (upstream keeps
/// the message arrays directly; pir's [`Inbox`] owns them in its consumer
/// task, so every mutation site mirrors the change here).
#[derive(Debug, Default)]
pub(crate) struct QueueMirror {
    pub(crate) steering: Vec<String>,
    pub(crate) follow_up: Vec<String>,
}

impl QueueMirror {
    /// Remove the first occurrence of each delivered text (the drain order
    /// matches the queue order).
    pub(crate) fn remove_delivered(queue: &mut Vec<String>, delivered: &[String]) {
        for text in delivered {
            if let Some(index) = queue.iter().position(|queued| queued == text) {
                queue.remove(index);
            }
        }
    }
}

/// Retry-tuning overrides applied to every stream request (embedder seam;
/// `None` fields keep the transport defaults).
///
/// The cell also owns the per-session provider-request identity and the
/// resolved transport preference (see the fields), and its [`Drop`] is the
/// session-teardown hook for provider-side session resources: the process-wide
/// Codex WebSocket pool is keyed by the request session id, so a disposed
/// session must close the sockets pooled under that id (upstream registers
/// `closeOpenAICodexWebSocketSessions` as the session resource cleanup).
/// A dedicated guard field cannot be added to [`AgentSession`] without spelling
/// it in every constructor, and `Drop for AgentSession` lives in another
/// module, so the session's existing per-request override cell carries both.
#[derive(Debug, Default)]
struct StreamRetryOverride {
    max_retries: Option<u32>,
    max_retry_delay_ms: Option<u64>,
    /// The provider-facing session identity carried as
    /// [`StreamOptions::session_id`] (upstream `sessionManager.getSessionId()`):
    /// the persisted session id, or one minted for a store-less
    /// (`--no-session`) session. Resolved on the first request and refreshed
    /// when a storage swap changes the persisted id; the outgoing id's pooled
    /// sockets are closed at that point.
    request_session_id: Option<String>,
    /// The transport preference resolved from the merged settings on the first
    /// request (upstream reads `settingsManager.getTransport()` /
    /// `getWebSocketConnectTimeoutMs()` once per session too).
    #[cfg(feature = "provider-openai")]
    transport: Option<TransportPreference>,
}

/// Provider transport preference resolved from settings (`transport` +
/// `websocketConnectTimeoutMs`).
#[cfg(feature = "provider-openai")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransportPreference {
    /// The effective `transport` setting (default `auto`).
    transport: crate::config::settings::TransportSetting,
    /// `websocketConnectTimeoutMs` as written: `None` keeps the adapter
    /// default, `Some(0)` disables the connect-handshake timeout.
    websocket_connect_timeout_ms: Option<u64>,
}

#[cfg(feature = "provider-openai")]
impl Default for TransportPreference {
    fn default() -> Self {
        Self {
            transport: crate::config::settings::TransportSetting::Auto,
            websocket_connect_timeout_ms: None,
        }
    }
}

impl Drop for StreamRetryOverride {
    fn drop(&mut self) {
        // Session teardown (upstream `cleanupSessionResources(this.sessionId)`
        // in `dispose()`): `Drop` cannot await, so the close is spawned on the
        // runtime that is still driving the process; with no runtime left the
        // sockets close with the process.
        #[cfg(feature = "provider-openai")]
        crate::ai::api::openai_responses::codex::codex_ws::spawn_close_codex_websocket_sessions(
            self.request_session_id.take(),
        );
    }
}
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
/// Serialized JSON for one conversation message (upstream camelCase shape
/// via pir's serde representation).
fn message_json(message: &Message) -> serde_json::Value {
    serde_json::to_value(message).unwrap_or(serde_json::Value::Null)
}

/// Extract the joined text content of a `before_agent_start` custom message
/// result (`{message: {content: [{type: "text", text}]}}`) or of a bare
/// message entry from the upstream `messages` array
/// (`{customType, content: [...], display, details}`).
fn extract_custom_message_text(result: &serde_json::Value) -> Option<String> {
    let message = result.get("message").unwrap_or(result);
    let content = message.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
fn user_message(text: &str, images: Vec<ContentBlock>) -> Message {
    let mut content = vec![ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })];
    content.extend(images);
    Message::User(UserMessage {
        role: UserRole::User,
        content,
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: crate::ai::util::now_ms(),
    })
}

/// One custom message as `pi.sendMessage()` sends it (upstream
/// `CustomMessage`): the unit `before_agent_start`'s `"custom"` variant carries
/// and the starting message of an idle `triggerTurn` run.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomRunMessage {
    /// The extension-defined type (`customType`), for renderers and filters.
    pub custom_type: String,
    /// The raw content value: a string, a content-block array, or `null`
    /// (normalized to empty, upstream normalizes at ingestion).
    pub content: JsonValue,
    /// Whether hosts render the message in the transcript.
    pub display: bool,
    /// Optional extension-defined details the renderer receives.
    pub details: Option<JsonValue>,
}

impl CustomRunMessage {
    /// Build one from a JS `sendMessage` message object.
    pub(super) fn from_json(message: &JsonValue) -> Option<Self> {
        let custom_type = message.get("customType").and_then(JsonValue::as_str)?;
        Some(Self {
            custom_type: custom_type.to_owned(),
            content: message
                .get("content")
                .cloned()
                .unwrap_or(JsonValue::Array(Vec::new())),
            display: message
                .get("display")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
            details: message.get("details").cloned().filter(|d| !d.is_null()),
        })
    }

    /// The joined text of the message content (upstream `contentText`).
    pub fn text(&self) -> String {
        content_text(&self.content)
    }

    /// The wire shape the `message_start`/`message_end` events and the
    /// `before_agent_start` `"custom"` variant carry (upstream
    /// `CustomMessage`: `role`, `customType`, `content`, `display`, `details`).
    pub fn to_json(&self) -> JsonValue {
        let mut value = serde_json::json!({
            "role": "custom",
            "customType": self.custom_type,
            "content": self.content,
            "display": self.display,
            "timestamp": crate::ai::util::now_ms(),
        });
        if let Some(details) = &self.details {
            value["details"] = details.clone();
        }
        value
    }
}

/// The model-visible form of a custom message: a user message whose content
/// blocks keep the custom message's ORDER (upstream `convertToLlm` renders a
/// `CustomMessage` as a user message and keeps its image blocks, which is what
/// makes the phone's `phone-inline-user-message` interleaving meaningful).
/// Settled fallback: content that is not a typed block array projects as its
/// joined text, exactly like before.
fn custom_run_user_message(message: &CustomRunMessage) -> Message {
    let mut content = crate::session_manager::projection::content_blocks(&message.content);
    if content.is_empty() {
        // Legacy/untyped content still projects as its joined text (an empty
        // payload keeps the historical empty text block).
        content.push(ContentBlock::Text(TextContent {
            text: message.text(),
            text_signature: None,
        }));
    }
    Message::User(UserMessage {
        role: UserRole::User,
        content,
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: crate::ai::util::now_ms(),
    })
}

/// Materialize a history snapshot as an owned vector: zero-copy when the
/// snapshot is otherwise unreferenced, one clone otherwise (reads that only
/// iterate, measure, or serialize keep the shared snapshot instead).
fn history_owned(snapshot: HistorySnapshot) -> Vec<Message> {
    Arc::try_unwrap(snapshot).unwrap_or_else(|snapshot| (*snapshot).clone())
}
/// Outcome of the `user_bash` extension event for a `!`-prefixed command
/// (upstream `UserBashEventResult`); re-exported from
/// [`crate::extensions::UserBashOutcome`].
pub use crate::extensions::UserBashOutcome;
