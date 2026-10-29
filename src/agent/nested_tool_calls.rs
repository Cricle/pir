//! Tool calls that a tool makes while it runs (upstream
//! `packages/coding-agent/src/core/nested-tool-calls.ts`, wired through
//! `ctx.executeTool`).
//!
//! A tool obtains a [`NestedToolExecutor`] through its [`ToolContext`] and
//! executes further tool calls on the model's behalf. Each child call gets the
//! id `<parentToolCallId>/<n>`, runs through the same hooks/scheduler path as
//! a model-issued call ([`run_tool_call`]), emits `tool_execution_*` events
//! carrying `parentToolCallId`, and leaves a bounded `nestedCalls` record on
//! the parent call's tool result.
//!
//! The record mirrors upstream `NESTED_CALL_LIMITS`: arguments over the
//! per-call or total size are omitted, calls beyond the count are dropped, and
//! the record is marked incomplete when any of that happens. On top of that
//! count bound, pir enforces an explicit depth cap because nested execution is
//! awaited recursively.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use crate::agent::tool_batch::{AgentToolCall, FinalizedToolCall, ToolResult};
use crate::agent::tool_execution::{
    finalized_tool_result_event_json, BoxFuture, ToolContext, ToolEventFaces, ToolHooks,
    ToolRegistry,
};
use crate::ai::types::{AbortSignal, Usage};

/// Limits of the nested-call record on a tool result (upstream
/// `NESTED_CALL_LIMITS`): arguments over the per-call or total size are
/// omitted, calls beyond the count are dropped, and the record is marked
/// incomplete when any of that happens.
pub struct NestedCallLimits;

impl NestedCallLimits {
    /// Maximum nested calls recorded per model-issued tool call (shared by
    /// all descendant scopes).
    pub const MAX_CALLS: usize = 256;
    /// Maximum UTF-8 JSON size of one nested call's arguments before the
    /// arguments are omitted from the record.
    pub const MAX_ARGUMENT_BYTES_PER_CALL: usize = 8 * 1024;
    /// Maximum accumulated argument bytes per model-issued tool call before
    /// further arguments are omitted from the record.
    pub const MAX_ARGUMENT_BYTES_TOTAL: usize = 32 * 1024;
    /// Maximum error text stored per nested call record.
    pub const MAX_ERROR_CHARS: usize = 500;
}

/// pir safety bound on nesting depth. Upstream bounds runaway nesting through
/// the shared 256-call record limit alone; pir additionally caps the awaited
/// call depth so deeply chained futures cannot exhaust the stack.
pub const MAX_NESTED_DEPTH: usize = 16;

/// Message shown when a nested call is refused because the depth cap was hit.
pub const DEPTH_EXCEEDED_MESSAGE: &str =
    "Nested tool call depth exceeded; refusing to execute a deeper nested call";

/// The scheduler's background request argument (the same key
/// `declared_tool_definitions` injects into every declared schema and
/// preflight takes out before validation). `ExecuteToolOptions::background`
/// expresses its request by splicing this key into the nested call's
/// arguments; the scheduler strips it before the tool runs.
const BACKGROUND_ARGUMENT: &str = "background";

/// Typed `tool_execution_*` event of a nested call (upstream adds
/// `parentToolCallId` to the wire payload).
#[derive(Debug, Clone)]
pub enum NestedToolEvent {
    /// `tool_execution_start` for a nested call.
    Start {
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
        parent_call_id: String,
    },
    /// `tool_execution_end` for a nested call.
    End {
        call_id: String,
        tool_name: String,
        is_error: bool,
        duration_ms: Option<u64>,
        parent_call_id: String,
        /// The finished result in upstream `AgentToolResult` wire shape.
        result: serde_json::Value,
    },
}

/// Async sink for nested `tool_execution_*` events. The agent loop installs
/// one that dispatches to the extension runtime and the run's event handler.
pub type NestedEventSink = Arc<dyn Fn(NestedToolEvent) -> BoxFuture<'static, ()> + Send + Sync>;

/// Per-call options of a nested execute (`ctx.executeTool(name, args,
/// options)`; `TOOL-BACKGROUND-SCOPE.md` §4).
///
/// The option travels beside the arguments (the prelude forwards it as an
/// explicit host-fn flag), so the arguments the tool receives are exactly the
/// caller's payload — validated against the tool's own schema — and the
/// scheduler's `background` argument is never smuggled into them from JS.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteToolOptions {
    /// `options.background`: hand this nested call to the session's detach
    /// sink. The awaited outcome is then the PLACEHOLDER (the batch answers
    /// immediately so the calling tool does not block on the call); the real
    /// result is delivered out of band by the sink's owner, and the
    /// `tool_result` event for this call id fires once, at completion.
    pub background: bool,
}

/// Outcome of one nested tool call.
#[derive(Debug, Clone)]
pub struct NestedToolOutcome {
    /// Generated child id (`<parentToolCallId>/<n>`).
    pub tool_call_id: String,
    /// Requested tool name.
    pub tool_name: String,
    /// Executed (or synthesized error) result.
    pub result: ToolResult,
    /// Whether the result is treated as an error.
    pub is_error: bool,
    /// Monotonic execution duration; `None` when the tool did not run.
    pub duration_ms: Option<u64>,
    /// Usage reported for this call (a `tool_result` handler merge); nested
    /// results are not persisted, so their usage is only counted through the
    /// recorder (upstream `outcome.result.usage`).
    pub usage: Option<Usage>,
}

/// Summary of the nested calls one model-issued call made, plus their
/// combined usage (upstream `NestedCallSummary`). `calls` becomes the tool
/// result's `nestedCalls` record; `usage` folds into the tool-result message.
#[derive(Debug, Default, Clone)]
pub struct NestedCallSummary {
    pub calls: Option<crate::ai::types::NestedToolCalls>,
    pub usage: Option<Usage>,
}

/// Sum two usage records field-by-field (upstream `combineUsage`).
pub fn combine_usage(a: Usage, b: Usage) -> Usage {
    // Optional splits are kept when EITHER side reports them; the missing
    // side contributes zero (upstream `...(x !== undefined ? {...} : {})`).
    let sum_opt =
        |a: Option<u64>, b: Option<u64>| a.map(|a| a.saturating_add(b.unwrap_or(0))).or(b);
    Usage {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cache_read: a.cache_read.saturating_add(b.cache_read),
        cache_write: a.cache_write.saturating_add(b.cache_write),
        cache_write_1h: sum_opt(a.cache_write_1h, b.cache_write_1h),
        reasoning: sum_opt(a.reasoning, b.reasoning),
        total_tokens: a.total_tokens.saturating_add(b.total_tokens),
        cost: crate::ai::types::Cost {
            input: a.cost.input + b.cost.input,
            output: a.cost.output + b.cost.output,
            cache_read: a.cost.cache_read + b.cost.cache_read,
            cache_write: a.cost.cache_write + b.cost.cache_write,
            total: a.cost.total + b.cost.total,
            // A sum is provider-reported only when every part is.
            total_from_provider: Some(
                a.cost.total_from_provider == Some(true)
                    && b.cost.total_from_provider == Some(true),
            ),
        },
    }
}

/// Bounded record of the nested calls of one model-issued tool call,
/// including calls its nested tools made (upstream `NestedCallRecorder`).
#[derive(Debug)]
pub struct NestedCallRecorder {
    calls: Vec<crate::ai::types::NestedToolCallRecord>,
    started_at: HashMap<String, std::time::Instant>,
    complete: bool,
    argument_bytes: usize,
    /// Combined usage of every recorded call (upstream `recorder.usage`);
    /// nested results are not persisted, so their usage is only counted here.
    usage: Option<Usage>,
}

impl Default for NestedCallRecorder {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            started_at: HashMap::new(),
            complete: true,
            argument_bytes: 0,
            usage: None,
        }
    }
}

impl NestedCallRecorder {
    /// Record a call as it starts. Returns `None` when the call is dropped
    /// (over the per-parent call count) and its arguments are omitted when
    /// they exceed the size limits; both mark the record incomplete.
    fn start(&mut self, call: &AgentToolCall) -> Option<usize> {
        if self.calls.len() >= NestedCallLimits::MAX_CALLS {
            self.complete = false;
            return None;
        }
        let mut record = crate::ai::types::NestedToolCallRecord {
            id: call.call_id.clone(),
            name: call.tool_name.clone(),
            arguments: None,
            arguments_bytes: None,
            status: crate::ai::types::NestedToolCallStatus::Unfinished,
            duration_ms: None,
            error: None,
        };
        // The size check measures the normalized payload (a non-object payload
        // is recorded as `{}`). Serializing the borrowed object yields exactly
        // the text a `Value::Object` copy of it would, so measuring needs no
        // copy; only the arguments actually recorded below are cloned.
        let object = call.arguments.as_object();
        let bytes = match object {
            Some(object) => serde_json::to_string(object)
                .map(|serialized| serialized.len())
                .unwrap_or(0),
            None => 2, // `{}`
        };
        if bytes > NestedCallLimits::MAX_ARGUMENT_BYTES_PER_CALL
            || self.argument_bytes + bytes > NestedCallLimits::MAX_ARGUMENT_BYTES_TOTAL
        {
            record.arguments_bytes = Some(u64::try_from(bytes).unwrap_or(u64::MAX));
            self.complete = false;
        } else {
            record.arguments = Some(
                object
                    .map(|object| {
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            self.argument_bytes += bytes;
        }
        self.started_at
            .insert(call.call_id.clone(), std::time::Instant::now());
        self.calls.push(record);
        Some(self.calls.len() - 1)
    }

    /// Mark a recorded call finished with its status, duration, and (for
    /// errors) the truncated result text.
    fn finish(&mut self, index: Option<usize>, is_error: bool, error_text: &str) {
        let Some(index) = index else {
            return;
        };
        let Some(record) = self.calls.get_mut(index) else {
            return;
        };
        let started = self
            .started_at
            .remove(&record.id)
            .unwrap_or_else(std::time::Instant::now);
        record.status = if is_error {
            crate::ai::types::NestedToolCallStatus::Error
        } else {
            crate::ai::types::NestedToolCallStatus::Ok
        };
        record.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        if is_error && !error_text.is_empty() {
            record.error = Some(
                error_text
                    .chars()
                    .take(NestedCallLimits::MAX_ERROR_CHARS)
                    .collect(),
            );
        }
    }

    /// Add one call's reported usage to the combined record (upstream
    /// `NestedCallRecorder.addUsage`).
    fn add_usage(&mut self, usage: Usage) {
        self.usage = Some(match self.usage.take() {
            Some(previous) => combine_usage(previous, usage),
            None => usage,
        });
    }

    /// Copy of the record so far, or `None` when no nested call was made.
    fn snapshot(&self) -> Option<crate::ai::types::NestedToolCalls> {
        if self.calls.is_empty() && self.complete {
            return None;
        }
        let calls = self.calls.clone();
        let complete = self.complete
            && calls
                .iter()
                .all(|call| call.status != crate::ai::types::NestedToolCallStatus::Unfinished);
        Some(crate::ai::types::NestedToolCalls { calls, complete })
    }
}

/// Shared state for every nested execution of one run.
pub struct NestedShared {
    registry: Arc<ToolRegistry>,
    /// The run's scheduler: nested calls reuse its queues and execution path.
    /// Behind a lock so [`NestedShared::configure_plugins`] can attach the
    /// unified plugin face after construction (the `tool_call`/`tool_result`
    /// contracts are dispatched by the scheduler itself at their upstream
    /// per-call timings).
    scheduler: std::sync::RwLock<crate::agent::tool_scheduler::ToolScheduler>,
    hooks: Option<Arc<ToolHooks>>,
    cwd: std::path::PathBuf,
    signal: Option<AbortSignal>,
    extensions: Option<crate::extensions::ExtensionHandle>,
    /// Whether every nested call runs exclusively (the agent executes tool
    /// calls sequentially).
    sequential: bool,
    sink: NestedEventSink,
    /// Session environment for tools that spawn agent runs (propagated to
    /// nested tool contexts).
    session_env: Option<Arc<crate::agent::tool_execution::ToolSessionEnv>>,
    /// Serializes nested calls that must not run concurrently (upstream's
    /// exclusive queue).
    queue: tokio::sync::Mutex<()>,
    /// Names tools may call through `ctx.executeTool()` (upstream
    /// `_getCallableTools`); `None` keeps the registry-wide default. A name
    /// missing from the set is refused with an error result (`model-only`
    /// exposure).
    callable: Option<Arc<std::collections::BTreeSet<String>>>,
    scopes: Mutex<HashMap<String, Arc<Mutex<ScopeState>>>>,
    /// The unified plugin host (native `tool_call` / `tool_result` handlers,
    /// ordered together with the JS face). Installed once by the run's host
    /// through [`NestedShared::configure_plugins`]; without it nested calls
    /// keep the historical JS-only route.
    plugins: std::sync::OnceLock<Option<Arc<crate::plugins::PluginHost>>>,
    /// MCP reach wait hook (the model-issued batch's `tool_call` readiness
    /// step). Nested calls run the scheduler directly, so they must invoke
    /// the same hook themselves before execution.
    tool_call_wait: std::sync::OnceLock<Option<crate::agent::agent_loop::ToolCallWaitFn>>,
    /// The run's HOST event handler, installed by the agent loop. Nested
    /// tools' live partials are announced as `tool_execution_update` on this
    /// stream with `parentToolCallId` set (upstream routes every tool's
    /// `onUpdate` through the same agent event sink). `None` for a service
    /// call without a loop event stream.
    host_events: std::sync::OnceLock<Option<crate::agent::agent_loop::EventHandler>>,
    /// The run's live turn counter (one-based), so nested updates carry the
    /// same `turn` as model-issued ones.
    turn: std::sync::OnceLock<Arc<std::sync::atomic::AtomicUsize>>,
}

impl NestedShared {
    /// Assemble the shared nested-call state for one agent run.
    ///
    /// `callable` restricts nested calls to those names (upstream
    /// `_getCallableTools`); `None` keeps the registry-wide default (every
    /// registered tool callable).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<ToolRegistry>,
        scheduler: crate::agent::tool_scheduler::ToolScheduler,
        hooks: Option<Arc<ToolHooks>>,
        extensions: Option<crate::extensions::ExtensionHandle>,
        cwd: std::path::PathBuf,
        signal: Option<AbortSignal>,
        sequential: bool,
        sink: NestedEventSink,
        session_env: Option<Arc<crate::agent::tool_execution::ToolSessionEnv>>,
        callable: Option<Arc<std::collections::BTreeSet<String>>>,
    ) -> Arc<Self> {
        // A host that builds no loop (`plugin_services`) passes a plain
        // scheduler: install the JS face here so nested calls still observe
        // `tool_call`/`tool_result`. The model-issued path (`agent_loop`)
        // already installed both faces on the scheduler it hands over; keep
        // that richer configuration.
        let scheduler = if scheduler.tool_event_faces().is_none() && extensions.is_some() {
            scheduler.with_tool_event_faces(ToolEventFaces::new(extensions.clone(), None))
        } else {
            scheduler
        };
        Arc::new(Self {
            registry,
            scheduler: std::sync::RwLock::new(scheduler),
            hooks,
            cwd,
            signal,
            extensions,
            sequential,
            sink,
            session_env,
            queue: tokio::sync::Mutex::new(()),
            callable,
            scopes: Mutex::new(HashMap::new()),
            plugins: std::sync::OnceLock::new(),
            tool_call_wait: std::sync::OnceLock::new(),
            host_events: std::sync::OnceLock::new(),
            turn: std::sync::OnceLock::new(),
        })
    }

    /// Install the unified plugin host and the MCP reach wait hook for this
    /// run (called by the agent loop, which owns both). Kept separate from
    /// [`Self::new`] so the long-standing constructor signature stays
    /// source-compatible for embedders and tests.
    // Wired by the run host (`agent_loop.rs`) after construction; this
    // module cannot reach the host itself.
    #[allow(dead_code)]
    pub(crate) fn configure_plugins(
        &self,
        plugins: Option<Arc<crate::plugins::PluginHost>>,
        tool_call_wait: Option<crate::agent::agent_loop::ToolCallWaitFn>,
    ) {
        // First install wins; `NestedShared` is created before any tool runs.
        let _ = self.plugins.set(plugins);
        let _ = self.tool_call_wait.set(tool_call_wait);
        // Attach the native face to the scheduler the nested calls execute
        // through. The JS face is already there (a scheduler handed over by
        // the loop) or installed by `new`; rebuilding with both keeps the
        // dispatch conditions accurate.
        if let Some(Some(plugins)) = self.plugins.get() {
            let faces = ToolEventFaces::new(self.extensions.clone(), Some(Arc::clone(plugins)));
            if faces.enabled() {
                let mut scheduler = self
                    .scheduler
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *scheduler = scheduler.clone().with_tool_event_faces(faces);
            }
        }
    }

    /// Install the run's host event handler and turn counter (called by the
    /// agent loop once, right after [`Self::configure_plugins`]). First
    /// install wins; a service call that never installs one keeps only its
    /// session/plugin update channel.
    pub(crate) fn configure_host_events(
        &self,
        events: Option<crate::agent::agent_loop::EventHandler>,
        turn: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let _ = self.host_events.set(events);
        let _ = self.turn.set(turn);
    }

    /// The live partial-result channel handed to a nested tool: announce the
    /// update on the host event stream (with this call's id as the parent),
    /// then forward the original payload to the session/plugin channel.
    fn nested_on_update(
        &self,
        parent_call_id: &str,
    ) -> Option<crate::agent::tool_execution::ToolUpdateFn> {
        let base = self
            .session_env
            .as_ref()
            .and_then(|env| env.on_update.clone());
        let events = self.host_events.get().cloned().flatten();
        if events.is_none() && base.is_none() {
            return None;
        }
        let turn = self
            .turn
            .get()
            .map(|turn| turn.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0);
        let caller_id = parent_call_id.to_owned();
        Some(Arc::new(move |payload: serde_json::Value| {
            let parts = crate::agent::tool_execution::tool_update_parts(&payload);
            if let Some(events) = &events {
                events(crate::agent::agent_loop::AgentEvent::ToolExecutionUpdate {
                    turn,
                    call_id: parts.call_id,
                    tool_name: parts.tool_name,
                    // The immediate caller is the parent; a stamped
                    // `parentToolCallId` (deeper wire shape) wins when present.
                    parent_call_id: parts.parent_call_id.or_else(|| Some(caller_id.clone())),
                    args: parts.args,
                    partial: parts.partial,
                });
            }
            if let Some(base) = base.as_ref() {
                base(payload);
            }
        }))
    }

    /// The run's MCP reach wait hook, when it has one.
    fn reach_wait(&self) -> Option<&crate::agent::agent_loop::ToolCallWaitFn> {
        self.tool_call_wait.get().and_then(|wait| wait.as_ref())
    }

    /// Remove and return the summary of the nested calls a model-issued call
    /// made (upstream `takeRecord`): the bounded call record plus the
    /// combined usage of every nested call.
    pub fn take_record(&self, tool_call_id: &str) -> Option<NestedCallSummary> {
        let scope = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(tool_call_id)?;
        let state = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recorder = state
            .recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(NestedCallSummary {
            calls: recorder.snapshot(),
            usage: recorder.usage,
        })
    }
}

/// Handle a tool uses to execute further tool calls (upstream
/// `ctx.executeTool`).
#[derive(Clone)]
pub struct NestedToolExecutor {
    shared: Arc<NestedShared>,
    /// Tool call id of the calling execution; nested call ids are
    /// `<caller_id>/<n>`.
    caller_id: String,
    /// Tool call id of the call that made THIS executor's call, when another
    /// tool made it (`ctx.executeTool`); `None` for a model-issued call. This
    /// is the `parentToolCallId` a detached nested call reports: the batch
    /// context carries the executor scoped to the nested call itself
    /// (`caller_id`), so the requesting id lives one level up.
    parent_call_id: Option<String>,
    /// Current nesting depth (0 for a model-issued call's own executor).
    depth: usize,
    /// Set inside a call that holds the exclusive queue, so its own nested
    /// calls do not wait on it (upstream `CallScope.holdsQueue`).
    holds_queue: bool,
    /// Recorder this call's nested calls are recorded into. Shared by all
    /// descendant scopes (upstream shares `scope.recorder`), so the record on
    /// the model-issued call's tool result includes calls its nested tools
    /// made.
    recorder: Arc<Mutex<NestedCallRecorder>>,
    /// Canonical mutation-path keys the call chain above this call holds
    /// (the scheduler's per-file mutation queue). A mutation tool on a path
    /// an ancestor genuinely locks must not re-lock it: the ancestor's guard
    /// spans its whole execution, so re-acquiring would self-deadlock.
    /// Contexts and executors sharing one chain share this set for reading;
    /// the scheduler publishes the keys a call itself acquired in a fresh
    /// scoped set ([`NestedToolExecutor::with_held_mutation_keys`]) after it
    /// took the locks, so an in-flight concurrent sibling is never visible
    /// here and cannot make another sibling skip the queue.
    held_mutation_keys: Arc<Mutex<BTreeSet<String>>>,
    /// Monotonic child-id counter for this call's nested calls. Shared by
    /// every clone of this executor (concurrent `ctx.executeTool` calls must
    /// get distinct ids) and kept outside the scope map, so removing an idle
    /// nested caller's scope cannot reset the counter and mint duplicate
    /// `<caller>/<n>` ids.
    next_child_id: Arc<std::sync::atomic::AtomicU64>,
    /// Abort signal of the call this executor is scoped to. A model-issued
    /// call starts from the run signal; the scheduler rebinds an adopted
    /// background call to its OWN signal via `with_signal`, and nested calls
    /// inherit that instead of silently falling back to the run signal.
    signal: Option<AbortSignal>,
}

impl std::fmt::Debug for NestedToolExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NestedToolExecutor")
            .field("caller_id", &self.caller_id)
            .field("parent_call_id", &self.parent_call_id)
            .field("depth", &self.depth)
            .field("holds_queue", &self.holds_queue)
            .finish()
    }
}

/// Per-calling-tool-call scope: the child id counter and the shared recorder.
#[derive(Debug, Default)]
struct ScopeState {
    recorder: Arc<Mutex<NestedCallRecorder>>,
    /// In-flight nested calls attributed to this caller id. Scopes created
    /// for a nested caller (depth > 0) are removed when this drops to zero;
    /// a model-issued caller's scope stays until [`NestedShared::take_record`]
    /// consumes it for the tool result.
    refs: usize,
}

/// Keeps one nested caller's scope entry alive for the duration of a nested
/// call and removes it once the last concurrent call for that caller
/// finishes.
///
/// Without it every call id that itself makes nested calls leaves a
/// `NestedShared.scopes` entry behind forever, pinning the shared recorder
/// (the ~32 KiB bounded record) for the whole run. The recorder itself is
/// shared with the model-issued call that owns the chain, so dropping this
/// scope entry loses nothing [`NestedShared::take_record`] would report.
struct NestedScopeGuard {
    shared: Arc<NestedShared>,
    caller_id: String,
    scope: Arc<Mutex<ScopeState>>,
}

impl Drop for NestedScopeGuard {
    fn drop(&mut self) {
        // Lock order: `scopes` before the scope's state, the same order
        // `execute_with_options` and `take_record` use.
        let mut scopes = self
            .shared
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.refs = state.refs.saturating_sub(1);
        if state.refs == 0 {
            // Remove only the entry this guard counted: `take_record` may
            // already have consumed it, and a later call for the same id
            // could have installed a fresh scope.
            let owned = scopes
                .get(&self.caller_id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.scope));
            if owned {
                scopes.remove(&self.caller_id);
            }
        }
    }
}

impl NestedToolExecutor {
    /// Create the root executor for one agent run (rebound per call by the
    /// scheduler via [`ToolContext::with_call_scope`]).
    pub fn root(shared: Arc<NestedShared>) -> Self {
        let signal = shared.signal.clone();
        Self {
            shared,
            caller_id: String::new(),
            parent_call_id: None,
            depth: 0,
            holds_queue: false,
            recorder: Arc::new(Mutex::new(NestedCallRecorder::default())),
            held_mutation_keys: Arc::new(Mutex::new(BTreeSet::new())),
            next_child_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            signal,
        }
    }

    /// Rebind this executor to a calling tool call id (the scheduler scopes
    /// each batch call this way). A model-issued call starts a fresh record
    /// and a fresh held-mutation-key set: sibling calls of one batch are
    /// independent chains, and sharing the set would let a sibling still in
    /// flight make another sibling skip the per-file mutation queue (it
    /// would then race the same file). The already-bound branch (a nested
    /// call re-scoped to its own id) keeps the same set, so nested calls
    /// still inherit — and skip — the keys their ancestor chain holds.
    pub fn for_call(&self, call_id: &str) -> Self {
        if self.caller_id == call_id {
            return self.clone();
        }
        Self {
            shared: Arc::clone(&self.shared),
            caller_id: call_id.to_owned(),
            // A fresh chain starts at a model-issued call, so there is no
            // requesting tool call above it.
            parent_call_id: None,
            depth: 0,
            holds_queue: false,
            recorder: Arc::new(Mutex::new(NestedCallRecorder::default())),
            // Fresh set: this call is a new chain, not a descendant of the
            // executor it was rebound from (in every current flow the batch
            // root, which holds no keys of its own).
            held_mutation_keys: Arc::new(Mutex::new(BTreeSet::new())),
            // A fresh model-issued call starts its own `<call>/<n>` sequence.
            next_child_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            signal: self.signal.clone(),
        }
    }

    /// Bind the abort signal owned by the call this executor is scoped to
    /// (the scheduler uses this when an adopted background call replaces its
    /// context signal).
    pub fn with_signal(mut self, signal: Option<AbortSignal>) -> Self {
        self.signal = signal;
        self
    }

    /// The shared state backing this executor (exposed for [`run_tool_call`]).
    pub fn shared(&self) -> &Arc<NestedShared> {
        &self.shared
    }

    /// The mutation-path keys this call chain already holds in the scheduler's
    /// per-file mutation queue (the scheduler skips re-locking them).
    pub fn held_mutation_keys(&self) -> &Arc<Mutex<BTreeSet<String>>> {
        &self.held_mutation_keys
    }

    /// Return a copy of this executor that propagates `held` as the
    /// mutation-path keys visible to the calls it makes. The scheduler calls
    /// this on the context it is about to run a mutation call with, after it
    /// acquired the per-file queue locks for that call: the acquired keys
    /// must be visible to the call's own descendants (so they skip
    /// re-locking a key their ancestor genuinely holds, instead of
    /// self-deadlocking), but never to concurrent siblings of the call,
    /// whose contexts keep the ancestor set.
    pub fn with_held_mutation_keys(&self, held: Arc<Mutex<BTreeSet<String>>>) -> Self {
        Self {
            held_mutation_keys: held,
            ..self.clone()
        }
    }

    /// The tool call id nested calls are attributed to.
    pub fn caller_id(&self) -> &str {
        &self.caller_id
    }

    /// The tool call id of the call that made this executor's call (`None`
    /// for a model-issued call). This is the requesting id a detached nested
    /// call reports as `parent_call_id` (`ctx.executeTool` semantics): the
    /// scheduler's batch context carries this executor scoped to the nested
    /// call itself, so the requesting id is the one recorded here when the
    /// child executor was created.
    pub fn parent_call_id(&self) -> Option<&str> {
        self.parent_call_id.as_deref()
    }

    /// Execute tool `name` with `args` on behalf of the calling tool call.
    ///
    /// The nested call gets the id `<callerId>/<n>`, runs through
    /// [`run_tool_call`] (extension gates plus before/after hooks), and emits
    /// `tool_execution_start`/`tool_execution_end` events carrying
    /// `parentToolCallId`. Never rejects for tool failures: unknown tools,
    /// validation errors, blocked calls, and failures come back as error
    /// results.
    pub async fn execute(&self, name: &str, args: serde_json::Value) -> NestedToolOutcome {
        self.execute_with_options(name, args, ExecuteToolOptions::default())
            .await
    }

    /// [`Self::execute`] with per-call options (`ctx.executeTool`'s third
    /// argument). With [`ExecuteToolOptions::background`] the call is routed
    /// through the scheduler's detach path: the returned outcome is the
    /// PLACEHOLDER the batch answered with, and the real result is delivered
    /// later by whoever adopted the detached call (the session's background
    /// sink) — including the `tool_result` event carrying this call's id.
    pub async fn execute_with_options(
        &self,
        name: &str,
        args: serde_json::Value,
        options: ExecuteToolOptions,
    ) -> NestedToolOutcome {
        if self.depth >= MAX_NESTED_DEPTH {
            return NestedToolOutcome {
                tool_call_id: String::new(),
                tool_name: name.to_owned(),
                result: ToolResult::error(DEPTH_EXCEEDED_MESSAGE),
                is_error: true,
                duration_ms: None,
                usage: None,
            };
        }
        // Exposure gate (upstream `_getCallableTools`): a restricted callable
        // set refuses names outside it, so `model-only` tools declared to the
        // model stay unreachable from other tools.
        if let Some(callable) = &self.shared.callable {
            if !callable.contains(name) {
                return NestedToolOutcome {
                    tool_call_id: String::new(),
                    tool_name: name.to_owned(),
                    result: ToolResult::error(format!(
                        "Tool '{name}' is not callable from other tools"
                    )),
                    is_error: true,
                    duration_ms: None,
                    usage: None,
                };
            }
        }
        // The background request is expressed through the scheduler's own
        // argument (preflight takes it out BEFORE validation, so the tool
        // never sees it and an `additionalProperties: false` schema accepts
        // it). Without an object payload there is nowhere to express it — the
        // request is refused instead of silently running in the foreground.
        if options.background && !args.is_object() {
            return NestedToolOutcome {
                tool_call_id: String::new(),
                tool_name: name.to_owned(),
                result: ToolResult::error(format!(
                    "Tool '{name}' cannot run in the background: its arguments must be a JSON object to carry the scheduler's \"background\" request."
                )),
                is_error: true,
                duration_ms: None,
                usage: None,
            };
        }
        let child_id = format!(
            "{}/{}",
            self.caller_id,
            self.next_child_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // Wire the caller's scope for `take_record` (model-issued calls) and
        // own it for the duration of this nested call (nested callers: the
        // guard deletes it at the last drop). The id counter above lives on
        // the executor, so deleting a scope never resets the sequence.
        let _scope_guard = {
            let mut scopes = self
                .shared
                .scopes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let scope = scopes
                .entry(self.caller_id.clone())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(ScopeState {
                        recorder: Arc::clone(&self.recorder),
                        refs: 0,
                    }))
                })
                .clone();
            if self.depth > 0 {
                let mut state = scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.refs += 1;
                Some(NestedScopeGuard {
                    shared: Arc::clone(&self.shared),
                    caller_id: self.caller_id.clone(),
                    scope: Arc::clone(&scope),
                })
            } else {
                None
            }
        };
        let mut tool_call = AgentToolCall::new(0, child_id.clone(), name, args);
        // The call is recorded into the shared recorder (descendants join the
        // same bounded record, upstream shares `scope.recorder`). The record
        // is taken BEFORE the scheduler argument is spliced in: it reports the
        // caller's arguments, not the scheduler's request flag.
        let record = {
            let mut recorder = self
                .recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorder.start(&tool_call)
        };
        // The event reports the caller's arguments, exactly like upstream's
        // `toolExecutionStart` (`toolCall.arguments` as supplied), so it is
        // snapshotted BEFORE the scheduler's request flag is spliced in: an
        // `ctx.executeTool(..., { background: true })` call must not surface a
        // `background` argument the caller never passed.
        let event_args = tool_call.arguments.clone();
        if options.background {
            if let Some(object) = tool_call.arguments.as_object_mut() {
                object.insert(
                    BACKGROUND_ARGUMENT.to_owned(),
                    serde_json::Value::Bool(true),
                );
            }
        }
        (self.shared.sink)(NestedToolEvent::Start {
            call_id: child_id.clone(),
            tool_name: tool_call.tool_name.clone(),
            args: event_args,
            parent_call_id: self.caller_id.clone(),
        })
        .await;

        // Serialize the call when the batch runs exclusively or the tool
        // itself is sequential — unless the caller already holds the queue.
        //
        // Lock order (A14): the queue is taken BEFORE the scheduler runs the
        // call, and the scheduler's mutation lock is acquired inside the
        // body. That order (queue -> mutation) is only safe if a chain that
        // already holds a mutation lock never waits for the queue: a
        // parallel mutation call holds its file lock while its tool body
        // makes a nested call, and the queue holder may itself be blocked on
        // that same mutation lock. A mutation-holding chain therefore skips
        // the queue (it is treated like a `holds_queue` chain), leaving the
        // only cross-lock edge one-directional. Descendants inherit
        // `holds_queue` and the held-key set, so they never re-enter the
        // queue either.
        let ancestor_holds_mutation = !self
            .held_mutation_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        let exclusive = !self.holds_queue
            && !ancestor_holds_mutation
            && (self.shared.sequential
                || self.shared.registry.get(name).is_some_and(|tool| {
                    tool.execution_mode() == crate::agent::tool_batch::ToolExecutionMode::Sequential
                }));
        let guard = if exclusive {
            Some(self.shared.queue.lock().await)
        } else {
            None
        };

        // The child executor shares this recorder so calls made by nested
        // tools join the same bounded record (upstream shares
        // `scope.recorder`).
        let child = Self {
            shared: Arc::clone(&self.shared),
            caller_id: child_id.clone(),
            parent_call_id: Some(self.caller_id.clone()),
            depth: self.depth + 1,
            holds_queue: self.holds_queue || exclusive,
            recorder: Arc::clone(&self.recorder),
            held_mutation_keys: Arc::clone(&self.held_mutation_keys),
            // The nested call's own `<call>/<n>` sequence starts at zero;
            // this executor is its only future-scoped owner.
            next_child_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            signal: self.signal.clone(),
        };
        let context = ToolContext {
            cwd: self.shared.cwd.clone(),
            signal: self.signal.clone(),
            // The child executor is shared, not copied: the scheduler scopes
            // it per call by replacing the context field (see
            // `ToolContext::with_call_scope`), and every consumer from here
            // on (the extension runtime bind, codemode) holds the `Arc`.
            nested: Some(Arc::new(child)),
            session_env: self.shared.session_env.clone(),
            held_mutation_keys: Some(Arc::clone(&self.held_mutation_keys)),
            on_update: self.shared.nested_on_update(&self.caller_id),
            // The scheduler flips this when it adopts the child call as a
            // background execution (same seam as a model-issued call).
            background: false,
        };
        let outcome = run_tool_call(
            &tool_call,
            &RunToolCallOptions {
                shared: &self.shared,
                parent_call_id: &self.caller_id,
            },
            &context,
        )
        .await;
        drop(guard);
        let finalized = outcome.finalized;
        let call_usage = outcome.usage;
        // A detached call is still RUNNING when this returns (its future was
        // handed to the sink): the record stays `unfinished` — exactly its
        // documented meaning — instead of reporting the placeholder as a
        // completed call. Its real outcome is delivered out of band.
        if outcome.detached.is_none() {
            let mut recorder = self
                .recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorder.finish(record, finalized.is_error(), &finalized.result.content);
            // Nested results are not persisted, so their usage is only
            // counted through the recorder (upstream nested-tool-calls.ts
            // 237-239).
            if let Some(usage) = call_usage {
                recorder.add_usage(usage);
            }
        }
        let is_error = finalized.is_error();
        let duration_ms = finalized.duration_ms;
        let tool_name = finalized.tool_name.clone();
        let usage = {
            let recorder = self
                .recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorder.usage
        };
        (self.shared.sink)(NestedToolEvent::End {
            call_id: child_id.clone(),
            tool_name: tool_name.clone(),
            is_error,
            duration_ms,
            parent_call_id: self.caller_id.clone(),
            result: finalized_tool_result_event_json(&finalized),
        })
        .await;

        NestedToolOutcome {
            tool_call_id: child_id,
            tool_name,
            result: finalized.result,
            is_error,
            duration_ms,
            usage,
        }
    }
}

/// Options for [`run_tool_call`]: the shared nested-call state a call runs
/// against, and the tool call id its events/results are attributed to.
pub struct RunToolCallOptions<'a> {
    pub shared: &'a NestedShared,
    /// Tool call id of the calling execution (`parentToolCallId` on gate
    /// payloads and `tool_execution_*` events).
    pub parent_call_id: &'a str,
}

/// What [`run_tool_call`] produced.
pub struct RunToolCallOutcome {
    /// The finalized call. When [`Self::detached`] is set this is the
    /// PLACEHOLDER the batch answered with (the real result is delivered by
    /// whoever adopted the call).
    pub finalized: FinalizedToolCall,
    /// Usage the `tool_result` handler merge reported (upstream
    /// `finalized.result.usage`).
    pub usage: Option<Usage>,
    /// The call the batch handed to the detach sink (an explicitly
    /// backgrounded nested call), if any. The caller's bookkeeping must treat
    /// the finalized result as a placeholder and leave the nested-call record
    /// pending.
    pub detached: Option<crate::agent::tool_batch::DetachedToolCall>,
}

/// Run one tool call through the same steps as a model-issued call: registry
/// lookup, `prepareArguments`/schema validation, extension `tool_call`,
/// `before` hook, execution (with the nested capability bound to the call's
/// id), `after` hook, and the extension `tool_result` notification. The
/// extension contracts run inside the scheduler at their upstream timings
/// (`tool_call` after validation, `tool_result` after the after-hook), which
/// is also what adds `parentToolCallId` to their payloads. Emits no events
/// and appends no messages — the caller owns event emission.
///
/// Never rejects for tool failures: unknown tools, validation errors, blocked
/// calls, and failures come back as error results (upstream `runToolCall`).
///
/// A call carrying the scheduler's `background` request is handed to the
/// session's detach sink by the scheduler: the returned finalized call is the
/// placeholder, the detached entry reports it, and — because the real result
/// is delivered out of band — NO `tool_result` notification fires here (the
/// completion fires it exactly once, with the same call id).
pub async fn run_tool_call(
    call: &AgentToolCall,
    options: &RunToolCallOptions<'_>,
    context: &ToolContext,
) -> RunToolCallOutcome {
    let shared = options.shared;
    let parent_call_id = options.parent_call_id;
    // MCP reach readiness (upstream the `mcp` extension's `tool_call`
    // handler): the model-issued batch waits for the servers a call may
    // reach before executing it, so their tools are registered. Nested calls
    // bypass that batch, so they run the same hook here.
    if let Some(wait_for_reach) = shared.reach_wait() {
        wait_for_reach(Arc::clone(&shared.registry), vec![call.clone()]).await;
    }
    // The scheduler dispatches the extension `tool_call` gate (after
    // validation) and `tool_result` (per-call finalize) itself; the shared
    // state installed both faces on it.
    let scheduler = {
        let scheduler = shared
            .scheduler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scheduler.clone()
    };
    let mut batch = scheduler
        // Owned hooks: the NestedShared holds them behind an `Arc`, so a
        // nested `{ background: true }` call can detach exactly like a
        // model-issued one (a borrowed-hook batch refuses instead).
        .execute_batch_with_owned_hooks(
            vec![call.clone()],
            &shared.registry,
            context,
            shared.hooks.clone(),
        )
        .await;
    // `batch.detached` is NOT dropped: a nested call that requested the
    // background (`ctx.executeTool(..., { background: true })`, or the
    // scheduler argument in the nested args) was adopted by the session's
    // detach sink, and the caller's bookkeeping (the nested-call record, the
    // awaited placeholder) depends on knowing that. A nested batch carries a
    // single call, so there is at most one entry.
    let detached = batch.detached.pop();
    debug_assert!(
        batch.detached.is_empty(),
        "a nested batch executes exactly one call"
    );
    if let Some(entry) = detached.as_ref() {
        tracing::info!(
            call_id = %entry.call_id,
            tool_name = %entry.tool_name,
            parent_call_id = %parent_call_id,
            "nested tool call handed to the background detach sink"
        );
    }
    let finalized = batch.results.pop().unwrap_or_else(|| FinalizedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        result: ToolResult::error("Tool call produced no result"),
        usage: None,
        duration_ms: None,
    });
    // The scheduler already applied the `tool_result` handler merge to the
    // finalized result (and carried the handler-reported usage on it). A
    // detached call's result is the placeholder and the scheduler skips its
    // notification: the completion fires the one `tool_result` event with the
    // REAL result and the same call id.
    let usage = if detached.is_some() {
        None
    } else {
        finalized.usage
    };
    RunToolCallOutcome {
        finalized,
        usage,
        detached,
    }
}
