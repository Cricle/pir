//! Parallel/sequential tool scheduling with concurrency limits, per-file
//! mutation serialization, cancellation, and upstream-compatible ordering.
//!
//! Behavior contract (mirrors `packages/agent/src/agent-loop.ts`):
//! - Preflight (lookup, argument validation, before-hook) runs in source order.
//! - A tool-level `Sequential` mode (or a global sequential mode) degrades the
//!   whole batch to sequential execution. Upstream has no batch-level
//!   side-effect degrade: mixed batches run in parallel by default.
//! - In parallel mode, allowed tools execute concurrently (bounded by
//!   `max_parallel`); `start` events keep source order while `end` events use
//!   completion order; results are always returned in source order.
//! - Mutations targeting the same canonical path serialize per file.
//! - One tool failing does not discard the other results (unless `fail_fast`).
//! - Cancellation marks remaining/not-yet-started calls as errors without
//!   abandoning already-in-flight futures.

mod execute;
mod prepare;

use std::sync::Arc;
use std::time::Duration;

use crate::agent::tool_batch::{
    AgentToolCall, FinalizedToolCall, ToolBatchOutcome, ToolExecutionEvent, ToolExecutionMode,
};
use crate::agent::tool_dependencies::MutationQueues;
use crate::agent::tool_execution::{ToolContext, ToolHooks, ToolRegistry};
// Re-exported so `with_detach` callers reach the sink type from this module.
pub use crate::agent::tool_execution::DetachSink;

/// Synchronous progress observer for one batch's tool lifecycle.
///
/// Installed with [`ToolScheduler::with_progress`]. The scheduler invokes it
/// as the events are produced, NOT after the batch:
/// - every [`ToolExecutionEvent::Start`] during preflight, in source order and
///   before the first call executes;
/// - every [`ToolExecutionEvent::End`] together with the [`FinalizedToolCall`]
///   for it (`Some` for ends, `None` for starts). Parallel batches report ends
///   in completion order; sequential batches in source order.
///
/// The callback runs synchronously on the batch's task, so it must not block
/// or await. Hosts that forward events to an async consumer use a non-blocking
/// channel send here and drain the channel concurrently.
pub type ToolExecutionProgress =
    Arc<dyn Fn(ToolExecutionEvent, Option<&FinalizedToolCall>) + Send + Sync>;

/// Configuration for a tool scheduler.
#[derive(Debug, Clone)]
pub struct ToolSchedulerConfig {
    /// Global execution mode; `Sequential` forces sequential execution.
    pub mode: ToolExecutionMode,
    /// Maximum concurrently executing tools in parallel mode.
    pub max_parallel: usize,
    /// When true, an error result stops the remaining calls of the batch
    /// (they are finalized as errors without executing).
    pub fail_fast: bool,
    /// Deprecated, retained for source compatibility: upstream has no
    /// batch-level side-effect degrade, so this knob no longer has any
    /// effect — mixed batches run in parallel by default and only a tool's
    /// own `executionMode: "sequential"` degrades the batch. Per-file
    /// mutations stay serialized through the mutation queue either way.
    pub allow_parallel_side_effects: bool,
    /// Optional ceiling for one foreground tool call, applied by the
    /// scheduler around the call's own execution future.
    ///
    /// `None` (the default) imposes no scheduler-level limit: a tool keeps
    /// whatever timer it owns (the shell tools' command timeout, transport
    /// deadlines, MCP request budgets), exactly as before this knob existed.
    /// When set, a call that exceeds the ceiling is finalized as an error
    /// result naming the tool and the limit, and the batch proceeds under its
    /// existing sequential/parallel and `fail_fast` semantics.
    ///
    /// Exclusions: background (`background: true`) calls outlive the turn by
    /// design and never take this ceiling, and nested `ctx.executeTool` calls
    /// run inside a parent call that owns its budget. Only a top-level
    /// model-issued call is bounded.
    pub call_timeout: Option<Duration>,
}

impl Default for ToolSchedulerConfig {
    fn default() -> Self {
        Self {
            mode: ToolExecutionMode::Parallel,
            max_parallel: 8,
            fail_fast: false,
            allow_parallel_side_effects: false,
            call_timeout: None,
        }
    }
}

/// Executes tool batches.
#[derive(Clone)]
pub struct ToolScheduler {
    config: ToolSchedulerConfig,
    queues: Arc<MutationQueues>,
    /// Session-installed background-call sink; `None` keeps `background: true`
    /// calls in the foreground (the argument is still stripped and hidden from
    /// the tool).
    detach: Option<DetachSink>,
    /// Per-batch lifecycle observer (see [`ToolExecutionProgress`]); `None`
    /// keeps the batch's events buffered in [`ToolBatchOutcome`] only.
    progress: Option<ToolExecutionProgress>,
    /// The batch's top-level executable set when the host declares one: the
    /// tools declared to the model plus the run's active `direct` tools.
    /// `codemode`/`deferred` tools are only executable at top level once
    /// declared; they stay reachable through nested `ctx.executeTool` calls,
    /// which have their own callable gate. A registered name outside it fails
    /// exactly like an absent tool. `None` keeps the registry-wide default
    /// (every registered tool executable).
    callable: Option<Arc<std::collections::BTreeSet<String>>>,
    /// Extension/plugin faces that observe the per-call contracts at their
    /// upstream timings: `tool_call` after schema validation and before
    /// execution, `tool_result` after the tool (and the `after` hook) and
    /// before that call's `tool_execution_end`. `None` (bare embedders)
    /// changes nothing.
    tool_events: Option<crate::agent::tool_execution::ToolEventFaces>,
}

impl std::fmt::Debug for ToolScheduler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The sinks are `Arc<dyn Fn>` and are not `Debug`; report their
        // presence only.
        formatter
            .debug_struct("ToolScheduler")
            .field("config", &self.config)
            .field("queues", &self.queues)
            .field("detach", &self.detach.is_some())
            .field("progress", &self.progress.is_some())
            .field("callable", &self.callable.as_ref().map(|names| names.len()))
            .field("tool_events", &self.tool_events.is_some())
            .finish()
    }
}

impl Default for ToolScheduler {
    fn default() -> Self {
        Self::new(ToolSchedulerConfig::default())
    }
}

impl ToolScheduler {
    /// Create a scheduler from configuration.
    pub fn new(config: ToolSchedulerConfig) -> Self {
        Self {
            config,
            queues: Arc::new(MutationQueues::default()),
            detach: None,
            progress: None,
            callable: None,
            tool_events: None,
        }
    }

    /// Install the extension/plugin faces that observe `tool_call` and
    /// `tool_result` (builder form).
    ///
    /// The scheduler dispatches them at the upstream per-call timings, so the
    /// emitted `tool_execution_end` already carries a handler-merged result.
    /// The run host installs this before handing the scheduler to nested
    /// calls; a scheduler without faces keeps the historical inert behavior.
    pub fn with_tool_event_faces(
        mut self,
        faces: crate::agent::tool_execution::ToolEventFaces,
    ) -> Self {
        self.tool_events = Some(faces);
        self
    }

    /// The installed faces, if any ([`crate::agent::tool_execution::ToolEventFaces`]).
    pub(crate) fn tool_event_faces(&self) -> Option<&crate::agent::tool_execution::ToolEventFaces> {
        self.tool_events.as_ref()
    }

    /// Restrict this scheduler's batches to `callable` — the top-level
    /// executable set (upstream `agent.state.tools` = the declared loadout,
    /// plus the run's active `direct` tools; `codemode`/`deferred` names stay
    /// reachable only through the nested `ctx.executeTool` gate). A call for a
    /// registered name outside the set fails with the scheduler's not-found
    /// result, exactly like an absent tool. Builder form: the agent loop
    /// installs it on the model-issued batch only, so nested `ctx.executeTool`
    /// calls keep their own callable gate.
    pub fn with_callable(mut self, callable: Arc<std::collections::BTreeSet<String>>) -> Self {
        self.callable = Some(callable);
        self
    }

    /// Install the batch lifecycle observer (builder form). The scheduler
    /// calls it synchronously while the batch runs — see
    /// [`ToolExecutionProgress`] for the exact ordering contract.
    pub fn with_progress(mut self, progress: ToolExecutionProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Install the sink that adopts `background: true` calls (the session's
    /// background registry). Builder form: the session installs it before the
    /// scheduler is shared, so every execution path — model-issued batches and
    /// nested calls — sees the same sink without a signature change.
    pub fn with_detach(mut self, sink: DetachSink) -> Self {
        self.detach = Some(sink);
        self
    }

    /// The configured global execution mode (nested-call queueing reads it).
    pub fn mode(&self) -> ToolExecutionMode {
        self.config.mode
    }

    /// Execute a single assistant message's tool calls, borrowing the batch's
    /// hooks.
    ///
    /// This entry cannot hand a detached call's future its hooks (a borrowed
    /// reference cannot outlive the batch), so it only detaches hook-less
    /// batches. Hosts that own the hooks behind an `Arc` — the agent loop,
    /// which shares them with the nested-call scope — use
    /// [`Self::execute_batch_with_owned_hooks`].
    pub async fn execute_batch(
        &self,
        calls: Vec<AgentToolCall>,
        registry: &ToolRegistry,
        context: &ToolContext,
        hooks: Option<&ToolHooks>,
    ) -> ToolBatchOutcome {
        let detach_hooks = if hooks.is_none() { Some(None) } else { None };
        self.execute_batch_inner(calls, registry, context, hooks, detach_hooks)
            .await
    }

    /// [`Self::execute_batch`] with the batch's hooks owned behind an `Arc`,
    /// so a detached call's future can hold them past the turn (the after-hook
    /// still applies to the real result).
    pub async fn execute_batch_with_owned_hooks(
        &self,
        calls: Vec<AgentToolCall>,
        registry: &ToolRegistry,
        context: &ToolContext,
        hooks: Option<Arc<ToolHooks>>,
    ) -> ToolBatchOutcome {
        let detach_hooks = Some(hooks.clone());
        self.execute_batch_inner(calls, registry, context, hooks.as_deref(), detach_hooks)
            .await
    }

    /// Shared body of both `execute_batch` entries.
    ///
    /// `detach_hooks` is the detached-call hook handle: `Some(None)` allows
    /// detaching a hook-less batch, `Some(Some(hooks))` carries the `Arc` a
    /// detached future clones, and `None` disables detaching for this batch
    /// (borrowed hooks only).
    ///
    /// The returned future is HEAP-BOXED: this is the recursion point of a
    /// nested `ctx.executeTool` chain (`nested_tool_calls::run_tool_call` ->
    /// this batch -> the tool -> `ctx.executeTool` -> ...), so the whole
    /// preflight + execution state machine (which is large in a debug build)
    /// must live on the heap instead of inline in every nesting level's poll
    /// stack. With it inline, `MAX_NESTED_DEPTH` (16) levels needed ~2.03 MiB
    /// of libtest's default 2 MiB thread stack — a ~30 KiB margin that any
    /// frame growth tips into an intermittent overflow. Boxing leaves only
    /// thin poll frames per level.
    fn execute_batch_inner<'a>(
        &'a self,
        calls: Vec<AgentToolCall>,
        registry: &'a ToolRegistry,
        context: &'a ToolContext,
        hooks: Option<&'a ToolHooks>,
        detach_hooks: Option<Option<Arc<ToolHooks>>>,
    ) -> crate::agent::tool_execution::BoxFuture<'a, ToolBatchOutcome> {
        Box::pin(async move {
            let aborted_flag = Arc::new(std::sync::atomic::AtomicBool::new(context.is_aborted()));
            // Phase 1: preflight each call in source order. Preflight consumes the
            // calls: it is their last reader, so the parallel fan-out below can
            // move each prepared call into its future (no argument copies).
            let (prepared, mut results, start_events) = prepare::prepare_calls(
                calls,
                registry,
                self.callable.as_deref(),
                context,
                hooks,
                &aborted_flag,
                self.progress.as_ref(),
                self.tool_events.as_ref(),
            )
            .await;

            // Preflight failures emit an immediate `tool_execution_end` for their
            // error result (upstream emits the end right after the failed
            // preparation, before any other call runs). A batch made by another
            // tool stamps its requesting call id on every start/end.
            let parent_call_id = context.parent_call_id().map(str::to_owned);
            let mut failed_end_events: Vec<crate::agent::tool_batch::ToolExecutionEvent> = results
                .iter()
                .map(
                    |finalized| crate::agent::tool_batch::ToolExecutionEvent::End {
                        index: finalized.index,
                        call_id: finalized.call_id.clone(),
                        tool_name: finalized.tool_name.clone(),
                        is_error: finalized.is_error(),
                        duration_ms: None,
                        parent_call_id: parent_call_id.clone(),
                    },
                )
                .collect();
            failed_end_events.sort_by_key(|event| match event {
                crate::agent::tool_batch::ToolExecutionEvent::End { index, .. } => *index,
                crate::agent::tool_batch::ToolExecutionEvent::Start { index, .. } => *index,
            });
            // `fail_fast` must also count a preflight failure as the "earlier
            // tool failed" that stops later calls. Preflight runs in source
            // order, so only prepared calls AFTER the first failed index are
            // skipped; calls before it still execute.
            let first_preflight_failure = results
                .iter()
                .filter(|result| result.is_error())
                .map(|result| result.index)
                .min();

            if prepared.is_empty() {
                results.sort_by_key(|result| result.index);
                return ToolBatchOutcome {
                    execution_start: start_events,
                    execution_end: failed_end_events,
                    results,
                    detached: Vec::new(),
                };
            }

            // Phase 2: sequential or semaphore-bounded parallel execution. The
            // prepared calls are handed over by value: preflight was their last
            // reader, so the parallel fan-out can move each call into its future
            // instead of deep-copying `validated_args`.
            //
            // A progress observer must see every preflight start before any call
            // executes; yield once so a concurrently draining consumer can deliver
            // them first (the starts are already in its queue). The no-observer
            // path does not yield, so its scheduling is unchanged.
            if self.progress.is_some() {
                tokio::task::yield_now().await;
            }
            let (end_events, executed, detached) = execute::execute_prepared(
                self,
                prepared,
                registry,
                context,
                hooks,
                &detach_hooks,
                &aborted_flag,
                self.progress.as_ref(),
                first_preflight_failure,
            )
            .await;

            results.extend(executed);
            results.sort_by_key(|result| result.index);
            let mut all_end_events = failed_end_events;
            all_end_events.extend(end_events);
            ToolBatchOutcome {
                execution_start: start_events,
                execution_end: all_end_events,
                results,
                detached,
            }
        })
    }
}

// Re-export the extraction helper used by the agent loop.
pub use crate::agent::tool_execution::tool_calls_from_message as extract_tool_calls;
// Re-exported crate-wide so the `tool-repair` builtin's loop hook
// (agent_session/tool_repair.rs) can inject the live-schema check the repair
// core takes as its validator without widening `prepare` itself. Gated with
// the feature: the hook is the only consumer.
#[cfg(feature = "tool-repair")]
pub(crate) use prepare::tool_repair_schema_check;
