//! Parallel/sequential execution of preflighted calls: semaphore-bounded
//! concurrency, event ordering, hooks, cancellation, and background detach.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::Semaphore;
use tracing::Instrument;

use super::prepare::PreparedCall;
use super::{ToolExecutionProgress, ToolScheduler};
use crate::agent::tool_batch::{
    DetachedToolCall, FinalizedToolCall, PreparedToolCall, ToolExecutionEvent, ToolExecutionMode,
    ToolResult,
};
use crate::agent::tool_execution::{
    dispatch_tool_result_merge, panic_payload_message, AfterToolCallResult, ToolContext, ToolHooks,
    ToolRegistry,
};

/// Execute the preflighted calls, returning the `end` events, the finalized
/// results in source order, and the calls a detach sink adopted.
///
/// A progress observer (when installed) is invoked per call as it completes:
/// parallel batches report completion order, sequential batches source order.
///
/// The returned future is HEAP-BOXED. This function sits on the nested
/// `ctx.executeTool` recursion path (`run_tool_call` -> the batch ->
/// `execute_or_detach` -> the tool -> `ctx.executeTool` -> ...), and its
/// state machine (the parallel `join_all` vector, the completed-results
/// buffers, the per-call futures) is large in a debug build. Inline, every
/// nesting level's poll stack carried a copy of it: `MAX_NESTED_DEPTH` (16)
/// levels needed ~2.03 MiB of libtest's default 2 MiB thread stack, a
/// ~30 KiB margin that any frame growth turned into an intermittent
/// overflow. Boxing keeps every level's contribution thin.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_prepared<'a>(
    scheduler: &'a ToolScheduler,
    prepared: Vec<PreparedCall>,
    registry: &'a ToolRegistry,
    context: &'a ToolContext,
    hooks: Option<&'a ToolHooks>,
    detach_hooks: &'a Option<Option<Arc<ToolHooks>>>,
    aborted_flag: &'a Arc<AtomicBool>,
    progress: Option<&'a ToolExecutionProgress>,
    first_preflight_failure: Option<usize>,
) -> crate::agent::tool_execution::BoxFuture<
    'a,
    (
        Vec<ToolExecutionEvent>,
        Vec<FinalizedToolCall>,
        Vec<DetachedToolCall>,
    ),
> {
    Box::pin(async move {
        // The requesting tool call id (`parentToolCallId`) every event and
        // extension payload this batch emits must carry. `None` for a
        // model-issued batch.
        let parent_call_id = context.parent_call_id();
        // A call that will be handed to the detach sink never executes in the
        // batch, so it must not take part in the batch degrade: a backgrounded
        // `bash` (`execution_mode() == Sequential`) must not make the turn wait.
        // A call that requested the background never executes inside the batch:
        // it is either adopted by the sink, refused by it, or refused up front
        // when the batch cannot detach (borrowed hooks / no sink). None of those
        // postures should force the rest of the batch to run sequentially.
        let detaches = |prepared: &PreparedCall| prepared.background;
        // Batch degrade: upstream only degrades to sequential when a tool's own
        // `executionMode` is `sequential` (agent-loop.ts:516-522). pir's former
        // side-effect batch degrade is removed — mixed batches run in parallel by
        // default; per-file mutations stay serialized through the mutation queue.
        let batch_sequential = scheduler.config.mode == ToolExecutionMode::Sequential
            || prepared.iter().any(|call| {
                call.call.execution_mode == ToolExecutionMode::Sequential && !detaches(call)
            });

        let mut results: Vec<FinalizedToolCall> = Vec::new();
        let mut end_events: Vec<ToolExecutionEvent> = Vec::new();
        let mut detached: Vec<DetachedToolCall> = Vec::new();
        if batch_sequential {
            // Phase 2: sequential execution in source order.
            let mut saw_failure = false;
            for prepared in prepared {
                // The flag is only the batch-start snapshot; the signal itself is
                // live, so a mid-batch abort (Esc) is observed before starting
                // each remaining call instead of only when the batch began.
                if aborted_flag.load(Ordering::Acquire) || context.is_aborted() {
                    // A start event was already emitted for this call (preflight
                    // emits every start up front), so the abandoned call must
                    // still close its lifecycle with an end event — the parallel
                    // branch below emits one for the identical case.
                    let finalized = finalize_observed(
                        scheduler,
                        &prepared.call,
                        context,
                        finalized_error(&prepared.call, "Operation aborted"),
                    )
                    .await;
                    record_end(
                        &mut end_events,
                        &mut results,
                        progress,
                        finalized,
                        parent_call_id,
                    );
                    continue;
                }
                if scheduler.config.fail_fast
                    && (saw_failure
                        || first_preflight_failure
                            .is_some_and(|failed| prepared.call.index > failed))
                {
                    let finalized = finalize_observed(
                        scheduler,
                        &prepared.call,
                        context,
                        finalized_error(
                            &prepared.call,
                            "Skipped because an earlier tool in the batch failed (fail-fast)",
                        ),
                    )
                    .await;
                    record_end(
                        &mut end_events,
                        &mut results,
                        progress,
                        finalized,
                        parent_call_id,
                    );
                    continue;
                }
                let finalized = scheduler
                    .execute_or_detach(
                        prepared,
                        registry,
                        context,
                        hooks,
                        detach_hooks,
                        &mut detached,
                    )
                    .await;
                saw_failure = saw_failure || finalized.is_error();
                record_end(
                    &mut end_events,
                    &mut results,
                    progress,
                    finalized,
                    parent_call_id,
                );
            }
        } else {
            // Phase 2: parallel execution bounded by max_parallel.
            // All futures are polled concurrently by `join_all`; the semaphore
            // caps how many may actually run at once. Aborted or fail-fast
            // calls return `None` without executing. Completion order is
            // captured via an atomic slot counter.
            let completion_counter = Arc::new(AtomicUsize::new(0));
            let fail_fast_hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // A batch can never run more calls at once than it contains, so
            // capping the semaphore at `prepared.len()` is behavior-preserving
            // and keeps a host-supplied `max_parallel` (common "unlimited"
            // sentinels like `usize::MAX`) from tripping Tokio's
            // `Semaphore::MAX_PERMITS` assertion on the production path.
            let max_parallel = scheduler
                .config
                .max_parallel
                .max(1)
                .min(prepared.len().max(1));
            let semaphore = Arc::new(Semaphore::new(max_parallel));

            let mut futures = Vec::with_capacity(prepared.len());
            // Each future owns its prepared call (the batch hands them over), so
            // the arguments are not copied per call.
            for prepared in prepared {
                let aborted = aborted_flag.clone();
                let fail_flag = fail_fast_hit.clone();
                let sem = semaphore.clone();
                let counter = completion_counter.clone();
                futures.push(async move {
                    let _permit = sem.acquire().await;
                    let mut adopted = Vec::new();
                    // Live signal check at the call's start point: a call still
                    // queued on the semaphore observes an abort that happened
                    // while it waited (the batch-start snapshot cannot).
                    let finalized = if aborted.load(Ordering::Acquire) || context.is_aborted() {
                        finalize_observed(
                            scheduler,
                            &prepared.call,
                            context,
                            finalized_error(&prepared.call, "Operation aborted"),
                        )
                        .await
                    } else if scheduler.config.fail_fast
                        && (fail_flag.load(Ordering::Acquire)
                            || first_preflight_failure
                                .is_some_and(|failed| prepared.call.index > failed))
                    {
                        finalize_observed(
                            scheduler,
                            &prepared.call,
                            context,
                            finalized_error(
                                &prepared.call,
                                "Skipped because an earlier tool in the batch failed (fail-fast)",
                            ),
                        )
                        .await
                    } else {
                        let result = scheduler
                            .execute_or_detach(
                                prepared,
                                registry,
                                context,
                                hooks,
                                detach_hooks,
                                &mut adopted,
                            )
                            .await;
                        if result.is_error() && scheduler.config.fail_fast {
                            fail_flag.store(true, Ordering::Release);
                        }
                        result
                    };
                    // Emit the end as soon as THIS call completes: parallel calls
                    // are observed in completion order, independent of the join.
                    if let Some(progress) = progress {
                        progress(end_event(&finalized, parent_call_id), Some(&finalized));
                    }
                    let slot = counter.fetch_add(1, Ordering::SeqCst);
                    (slot, finalized, adopted)
                });
            }
            let mut completed: Vec<(usize, FinalizedToolCall, Vec<DetachedToolCall>)> =
                futures_util::future::join_all(futures)
                    .await
                    .into_iter()
                    .collect();
            // End events use completion order (slot order).
            completed.sort_by_key(|(slot, _, _)| *slot);
            end_events = completed
                .iter()
                .map(|(_, finalized, _)| end_event(finalized, parent_call_id))
                .collect();
            // Results are reassembled in assistant source order.
            let mut indexed: Vec<FinalizedToolCall> = completed
                .into_iter()
                .map(|(_, finalized, adopted)| {
                    detached.extend(adopted);
                    finalized
                })
                .collect();
            indexed.sort_by_key(|finalized| finalized.index);
            results.extend(indexed);
        }

        (end_events, results, detached)
    })
}

/// Record one sequential/synthesized end: report it to the progress observer
/// (when installed) at production time, then keep the event in the batch
/// report and the result in source order.
fn record_end(
    end_events: &mut Vec<ToolExecutionEvent>,
    results: &mut Vec<FinalizedToolCall>,
    progress: Option<&ToolExecutionProgress>,
    finalized: FinalizedToolCall,
    parent_call_id: Option<&str>,
) {
    let event = end_event(&finalized, parent_call_id);
    if let Some(progress) = progress {
        progress(event.clone(), Some(&finalized));
    }
    end_events.push(event);
    results.push(finalized);
}

/// Apply the extension `tool_result` contract to one finalized call at its
/// finalize point, right before the call's `tool_execution_end` is emitted.
///
/// This is the upstream ordering (`afterToolCall` runs after the tool and
/// before the end event) and is why the end frame carries the handler-merged
/// content/isError and usage. A scheduler without faces passes through.
async fn finalize_observed<'a>(
    scheduler: &'a ToolScheduler,
    call: &'a PreparedToolCall,
    context: &'a ToolContext,
    finalized: FinalizedToolCall,
) -> FinalizedToolCall {
    finalize_observed_input(scheduler, Some(&call.validated_args), context, finalized).await
}

/// [`finalize_observed`] for a call whose executed payload is known without a
/// `PreparedToolCall` (a detached call the sink refused keeps its validated
/// arguments on the [`DetachedToolCall`](crate::agent::tool_batch::DetachedToolCall)).
///
/// The returned future is HEAP-BOXED: this sits on the nested
/// `ctx.executeTool` recursion path (the extension dispatch future is large),
/// and `execute_or_detach` is itself a boxed future whose state must stay
/// thin at every nesting level.
fn finalize_observed_input<'a>(
    scheduler: &'a ToolScheduler,
    input: Option<&'a serde_json::Value>,
    context: &'a ToolContext,
    mut finalized: FinalizedToolCall,
) -> crate::agent::tool_execution::BoxFuture<'a, FinalizedToolCall> {
    Box::pin(async move {
        if let Some(faces) = scheduler.tool_event_faces() {
            let parent_call_id = context.parent_call_id();
            dispatch_tool_result_merge(faces, input, parent_call_id, &mut finalized).await;
        }
        finalized
    })
}

/// One `tool_execution_end` event for a finalized call, stamped with the
/// requesting tool call id when another tool issued it.
pub(super) fn end_event(
    finalized: &FinalizedToolCall,
    parent_call_id: Option<&str>,
) -> ToolExecutionEvent {
    ToolExecutionEvent::End {
        index: finalized.index,
        call_id: finalized.call_id.clone(),
        tool_name: finalized.tool_name.clone(),
        is_error: finalized.is_error(),
        duration_ms: finalized.duration_ms,
        parent_call_id: parent_call_id.map(str::to_owned),
    }
}

/// Build an error `FinalizedToolCall` from a prepared call (post-preflight).
fn finalized_error(call: &PreparedToolCall, message: impl Into<String>) -> FinalizedToolCall {
    FinalizedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        result: ToolResult::error(message),
        usage: None,
        duration_ms: None,
    }
}

/// The placeholder answer for an adopted background call.
///
/// The provider requires a `tool_result` for every `tool_use` (see
/// `src/ai/api/transform.rs`'s orphan synthesis), so the batch answers
/// immediately; the real result is delivered by whoever adopted the call. The
/// placeholder never requests termination.
fn background_placeholder(call: &DetachedToolCall) -> FinalizedToolCall {
    let mut result = ToolResult::ok(format!(
        "Tool '{}' is running in the background (callId: {}). Its result will arrive as a separate message; do not wait for it.",
        call.tool_name, call.call_id
    ));
    result.details = Some(serde_json::json!({
        "background": true,
        "callId": call.call_id,
        "toolName": call.tool_name,
    }));
    FinalizedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        result,
        usage: None,
        duration_ms: None,
    }
}

/// The refusal for a background call this batch cannot detach at all (no sink,
/// or borrowed hooks that cannot outlive the turn): the call is NOT executed in
/// the foreground — the model can retry it inline.
fn background_unavailable(
    call: &crate::agent::tool_batch::PreparedToolCall,
    reason: &str,
) -> FinalizedToolCall {
    FinalizedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        result: ToolResult::error(format!(
            "Tool '{}' was not started: background execution is not available in this host ({reason}). Retry without \"background\": true.",
            call.tool_name
        )),
        usage: None,
        duration_ms: None,
    }
}

/// The refusal answer for a background call the sink declined (its background
/// limit is full): the call is NOT executed in the foreground — the model can
/// retry it inline.
fn background_refused(call: &DetachedToolCall) -> FinalizedToolCall {
    FinalizedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        result: ToolResult::error(format!(
            "Tool '{}' was not started: the background call limit is reached. Wait for a running background call to finish, or retry without \"background\": true.",
            call.tool_name
        )),
        usage: None,
        duration_ms: None,
    }
}

/// Build the tool-run closure: clones the context and the tool handle per
/// invocation and awaits the tool (used by the mutation-queue branches above).
/// The closure runs exactly once and hands its arguments to the tool future by
/// value, so the payload is not copied again.
///
/// The arguments travel as an `Arc` so the call's partial-update binding (see
/// `run_call_body`) can share the exact payload the tool reads instead of
/// keeping a second full copy alive for the whole call, most of which never
/// emits an update.
fn run_closure<'a>(
    args: std::sync::Arc<serde_json::Value>,
    context: &'a ToolContext,
    tool: &'a Arc<dyn crate::agent::tool_execution::AgentTool>,
) -> impl FnOnce() -> crate::agent::tool_execution::BoxFuture<'static, ToolResult> + 'a {
    move || {
        let ctx = context.clone();
        let tool = Arc::clone(tool);
        Box::pin(async move { tool.run(args.as_ref(), &ctx).await })
    }
}

/// Await one tool future with panic isolation: a panicking third-party or
/// plugin tool becomes a regular error result for that call instead of
/// unwinding the batch future (and, in parallel mode, every sibling's).
async fn run_tool_isolated<F>(tool_name: &str, future: F) -> ToolResult
where
    F: std::future::Future<Output = ToolResult>,
{
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            let message = panic_payload_message(payload.as_ref());
            tracing::error!(
                tool = %tool_name,
                panic = %message,
                "tool panicked; isolating it as an error result"
            );
            ToolResult::error(format!("Tool '{tool_name}' panicked: {message}"))
        }
    }
}

impl ToolScheduler {
    /// Whether a batch may hand calls to the detach sink: a sink is installed.
    ///
    /// Both call sites gate on the CALL's own background request
    /// (`PreparedCall::background`, taken out of the call's arguments by
    /// preflight), so this admits exactly the explicitly-backgrounded calls —
    /// model-issued ones and nested `ctx.executeTool(name, args, { background:
    /// true })` calls alike. A nested call that did not request the background
    /// never detaches: no other input can reach the detach branch.
    ///
    /// A detached nested call reports its requesting call id through
    /// [`ToolScheduler::build_detached_call`] (the batch context's nested
    /// executor carries it), which is what makes the relaxation safe.
    pub(super) fn detach_enabled(&self, _context: &ToolContext) -> bool {
        self.detach.is_some()
    }

    /// Execute one prepared call, or hand it to the detach sink when it asked
    /// for the background, a sink is installed, and the sink adopts it.
    ///
    /// An adopted call is appended to `detached` (its future taken over by the
    /// sink) and answered with the placeholder; a call the sink refuses is
    /// finalized with the refusal error and never runs in the foreground, so a
    /// host that is at its background limit does not silently block the turn.
    ///
    /// The returned future is HEAP-BOXED: this frame sits between the batch
    /// and [`run_call_body`] on the nested `ctx.executeTool` recursion path,
    /// and its own state (the detach branches, the `tool.exec` span, the
    /// timeout delegation) is carried on the poll stack of every nesting
    /// level. Boxing it keeps each level's contribution thin.
    fn execute_or_detach<'a>(
        &'a self,
        prepared: PreparedCall,
        registry: &'a ToolRegistry,
        context: &'a ToolContext,
        hooks: Option<&'a ToolHooks>,
        detach_hooks: &'a Option<Option<Arc<ToolHooks>>>,
        detached: &'a mut Vec<DetachedToolCall>,
    ) -> crate::agent::tool_execution::BoxFuture<'a, FinalizedToolCall> {
        Box::pin(async move {
            // Per-call span (upstream `TelemetryContext.startSpan("tool.exec")`):
            // nests under the active `agent.turn` span. Async-correct: the span
            // wraps the execution via `Instrument`, no guard across await points.
            let span = tracing::info_span!(
                "tool.exec",
                tool = %prepared.call.tool_name,
                call_id = %prepared.call.call_id
            );
            if prepared.background {
                if let Some(tool) = registry.get(&prepared.call.tool_name) {
                    match (self.detach.as_ref(), detach_hooks.as_ref()) {
                        (Some(sink), Some(handle)) if self.detach_enabled(context) => {
                            let mut call = self.build_detached_call(
                                handle.clone(),
                                prepared.call,
                                tool,
                                context,
                            );
                            if sink(&mut call) {
                                let placeholder = background_placeholder(&call);
                                detached.push(call);
                                // The placeholder never ran: the real
                                // completion fires this call id's
                                // `tool_result` exactly once.
                                return placeholder;
                            }
                            let refused = background_refused(&call);
                            return finalize_observed_input(
                                self,
                                Some(&call.args),
                                context,
                                refused,
                            )
                            .await;
                        }
                        // The batch cannot hand the call's future anywhere it
                        // outlives the turn (a borrowed-hook host). Running it in
                        // the foreground anyway would block the turn for up to the
                        // command timeout while the model was told not to wait —
                        // refuse loudly instead (the nested path has the same
                        // contract).
                        (None, _) => {
                            let unavailable = background_unavailable(
                                &prepared.call,
                                "no background detach sink is installed",
                            );
                            return finalize_observed(self, &prepared.call, context, unavailable)
                                .await;
                        }
                        (_, None) => {
                            let unavailable = background_unavailable(
                                &prepared.call,
                                "the batch's hooks cannot outlive the turn",
                            );
                            return finalize_observed(self, &prepared.call, context, unavailable)
                                .await;
                        }
                        _ => {
                            let unavailable =
                                background_unavailable(&prepared.call, "detaching is disabled");
                            return finalize_observed(self, &prepared.call, context, unavailable)
                                .await;
                        }
                    }
                }
                // Unknown tool: the normal not-found result below is the right
                // answer (nothing would have run either way).
            }
            let Some(tool) = registry.get(&prepared.call.tool_name) else {
                tracing::warn!(
                    error = "tool not found in registry",
                    "tool execution failed"
                );
                let not_found = async {
                    FinalizedToolCall {
                        index: prepared.call.index,
                        call_id: prepared.call.call_id.clone(),
                        tool_name: prepared.call.tool_name.clone(),
                        result: ToolResult::error(format!(
                            "Tool {} not found",
                            prepared.call.tool_name
                        )),
                        usage: None,
                        duration_ms: None,
                    }
                }
                .instrument(span)
                .await;
                return finalize_observed(self, &prepared.call, context, not_found).await;
            };
            let Some(limit) = foreground_call_timeout(self, context) else {
                let finalized = run_call_body(&self.queues, &prepared.call, &tool, context, hooks)
                    .instrument(span)
                    .await;
                return finalize_observed(self, &prepared.call, context, finalized).await;
            };
            // The timed body is a heap-allocated future (see
            // `run_with_call_timeout`): nested tool calls await this path
            // recursively, and an inline timer state machine would grow the
            // stack frame of every nesting level.
            let finalized = run_with_call_timeout(
                &self.queues,
                &prepared.call,
                &tool,
                context,
                hooks,
                limit,
                span,
            )
            .await;
            finalize_observed(self, &prepared.call, context, finalized).await
        })
    }

    /// Build the detached call a sink adopts: the SAME execution body a
    /// foreground call runs, owned so it can outlive the turn, on the call's
    /// OWN abort signal (never the run's — aborting the turn leaves a
    /// background call running, and cancelling the call leaves the turn
    /// alone). A `tool.exec` span is attached to the future, created when it
    /// is polled (the batch is long unwound by then).
    fn build_detached_call(
        &self,
        hooks: Option<Arc<ToolHooks>>,
        call: PreparedToolCall,
        tool: Arc<dyn crate::agent::tool_execution::AgentTool>,
        context: &ToolContext,
    ) -> DetachedToolCall {
        let signal = crate::ai::types::AbortSignal::new();
        let mut call_context = context.clone();
        call_context.signal = Some(signal.clone());
        // The tool must know it is detached: the shell tools skip the
        // settings-imposed command timeout for a call that is expected to
        // outlive the turn (an explicit per-call timeout still applies).
        call_context.background = true;
        // A detached body is a separate task that may run concurrently with
        // the call that detached it. It must not inherit the caller chain's
        // held mutation keys: skipping a queue lock because an ancestor
        // "holds" it is only valid inside the same awaited execution. Clear
        // both copies (the context's snapshot and the scoped nested
        // executor's set) so `run_call_body` acquires every mutation lock
        // the detached call targets; if the ancestor is still running, the
        // queue serializes the two bodies instead of letting them write the
        // same file concurrently.
        call_context.held_mutation_keys = None;
        if let Some(nested) = call_context.nested.take() {
            let empty = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
            call_context.nested = Some(Arc::new(nested.with_held_mutation_keys(empty)));
        }
        let queues = Arc::clone(&self.queues);
        // The report copy of the arguments: the execution takes the call by
        // value (it must own everything it runs with), so the adopted call
        // keeps its own copy for hosts that render it.
        let args = call.validated_args.clone();
        // The report fields are read before the future takes the call by value
        // (the execution body owns everything it runs with).
        let index = call.index;
        let call_id = call.call_id.clone();
        let tool_name = call.tool_name.clone();
        let span = tracing::info_span!(
            "tool.exec",
            tool = %call.tool_name,
            call_id = %call.call_id
        );
        let future: crate::agent::tool_execution::BoxFuture<'static, ToolResult> = Box::pin(
            async move {
                // The context clone is owned by the future; the borrowed view
                // the body takes lives only inside it. The batch's own
                // bookkeeping (index/ids/duration) stays behind: the adopter
                // owns the call's identity and reports the delivery.
                let hooks = hooks.as_deref();
                run_call_body(&queues, &call, &tool, &call_context, hooks)
                    .await
                    .result
            }
            .instrument(span),
        );
        DetachedToolCall {
            index,
            call_id,
            tool_name,
            args,
            // The requesting call id when another tool made this call
            // (`ctx.executeTool`); `None` for model-issued calls. The batch
            // context's nested executor is scoped to this call, so the
            // requesting id is the parent recorded on it when the child scope
            // was created — the same id the call's gate payload and
            // `tool_execution_*` events report as `parentToolCallId`.
            parent_call_id: context
                .nested
                .as_ref()
                .and_then(|nested| nested.parent_call_id().map(str::to_owned)),
            abort: signal,
            future: Some(future),
        }
    }
}

/// The scheduler-imposed ceiling for this call, or `None` when the host
/// configured none or the call must not be bounded by one.
///
/// Background (`background: true`) calls are finalized by
/// [`ToolScheduler::execute_or_detach`] before it reaches this helper — they
/// are adopted by the detach sink or refused, never executed inside the
/// batch — so they can never take this ceiling. Nested `ctx.executeTool`
/// calls run inside a parent tool's execution, whose own budget already
/// covers them, and are excluded here: the batch context's nested executor
/// carries the requesting call id exactly when another tool made the call
/// (`None` for a model-issued one).
fn foreground_call_timeout(scheduler: &ToolScheduler, context: &ToolContext) -> Option<Duration> {
    let limit = scheduler.config.call_timeout?;
    let nested = context.parent_call_id().is_some();
    (!nested).then_some(limit)
}

/// Human-readable limit for the timeout result (`500ms`, `2s`, `1.5s`).
fn format_timeout(limit: Duration) -> String {
    let millis = limit.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{:.1}s", limit.as_secs_f64())
    }
}

/// Foreground execution under the scheduler's per-call ceiling, returned as a
/// heap-allocated future.
///
/// Kept out of [`ToolScheduler::execute_or_detach`]'s own async body on
/// purpose: nested `ctx.executeTool` calls await the batch recursively, so an
/// inline timer state machine would add its footprint to every nesting
/// level's stack frame. The timer races the call's own execution future;
/// timing out drops (cancels) it, so the rest of the batch is not blocked by
/// a tool that never resolves. A concurrent abort wins over the timeout: a
/// cancelled run is finalized as aborted instead of reporting a spurious
/// timeout, even when both are observable at the same moment.
#[inline(never)]
fn run_with_call_timeout<'a>(
    queues: &'a crate::agent::tool_dependencies::MutationQueues,
    call: &'a PreparedToolCall,
    tool: &'a Arc<dyn crate::agent::tool_execution::AgentTool>,
    context: &'a ToolContext,
    hooks: Option<&'a ToolHooks>,
    limit: Duration,
    span: tracing::Span,
) -> crate::agent::tool_execution::BoxFuture<'a, FinalizedToolCall> {
    Box::pin(async move {
        let started_at = std::time::Instant::now();
        let run = run_call_body(queues, call, tool, context, hooks).instrument(span);
        match tokio::time::timeout(limit, run).await {
            Ok(finalized) => finalized,
            Err(_) => {
                let message = if context.is_aborted() {
                    "Operation aborted".to_owned()
                } else {
                    format!(
                        "Tool '{}' timed out after {} (scheduler per-call limit) and was cancelled. Retry the call, or split the work into smaller calls.",
                        call.tool_name,
                        format_timeout(limit)
                    )
                };
                FinalizedToolCall {
                    index: call.index,
                    call_id: call.call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    result: ToolResult::error(message),
                    usage: None,
                    duration_ms: Some(
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ),
                }
            }
        }
    })
}

/// Run one prepared call to completion: the per-call context (nested-call
/// scope, live-update binding), the per-file mutation locks, and the
/// after-hook. Shared by the foreground path (which borrows the batch's
/// context and hooks) and a detached call's future (which owns them and
/// derives the borrowed views inside the future). The caller supplies the
/// `tool.exec` span.
///
/// Mutation-lock scope (the property that makes backgrounding a mutation tool
/// safe): the per-file locks are taken HERE, inside the call's own execution
/// future — for a detached call that future is built by
/// [`ToolScheduler::build_detached_call`] and only polled after the sink
/// spawned it, long after the batch that adopted it returned. The guards live
/// in `MutationQueues::run_locked`'s scope, which ends when the body returns
/// (and are dropped with the future if it is ever cancelled), so a background
/// mutation call holds a file's lock exactly across its own run — never
/// across a turn, and never before it is polled.
///
/// The returned future is HEAP-BOXED: the body is the deepest frame on the
/// nested `ctx.executeTool` recursion path, and its state machine (the
/// per-call context clone, the live-update closure, the mutation-lock branch
/// and the awaited tool future) is large in a debug build. Inline, sixteen
/// nesting levels each carried a copy on the poll stack; boxing it makes each
/// level's contribution a pointer-sized poll frame.
fn run_call_body<'a>(
    queues: &'a crate::agent::tool_dependencies::MutationQueues,
    call: &'a PreparedToolCall,
    tool: &'a Arc<dyn crate::agent::tool_execution::AgentTool>,
    context: &'a ToolContext,
    hooks: Option<&'a ToolHooks>,
) -> crate::agent::tool_execution::BoxFuture<'a, FinalizedToolCall> {
    Box::pin(async move {
        let idx = call.index;
        let call_id = call.call_id.clone();
        let tool_name = call.tool_name.clone();

        // One shared payload: the tool future reads it through the `Arc`, and
        // the partial-update binding below holds another handle instead of a
        // second full clone of arguments that may never be used (most tools
        // never emit an update).
        let args = std::sync::Arc::new(call.validated_args.clone());
        // Bind the nested-call scope to this call so tools executing further
        // tool calls record against it (upstream scopes by tool call id).
        let mut context = context.with_call_scope(&call.call_id);
        // Live partial updates carry the call's identity so hosts can route
        // them to the right tool block (upstream `tool_execution_update`
        // events carry toolCallId/toolName/args; the tool-level payload has no
        // ids of its own).
        if let Some(on_update) = context.on_update.take() {
            let call_id = call.call_id.clone();
            let tool_name = call.tool_name.clone();
            let args = std::sync::Arc::clone(&args);
            context.on_update = Some(std::sync::Arc::new(
                move |mut partial: serde_json::Value| {
                    if let Some(object) = partial.as_object_mut() {
                        object.insert(
                            "toolCallId".to_owned(),
                            serde_json::Value::String(call_id.clone()),
                        );
                        object.insert(
                            "toolName".to_owned(),
                            serde_json::Value::String(tool_name.clone()),
                        );
                        if !object.contains_key("args") {
                            // Clone only when an update actually fires.
                            object.insert("args".to_owned(), (*args).clone());
                        }
                    }
                    on_update(partial);
                },
            ));
        }
        let started_at = std::time::Instant::now();
        let result = {
            if call.mutation_paths.is_empty() {
                let run = run_closure(std::sync::Arc::clone(&args), &context, tool);
                run_tool_isolated(&tool_name, run()).await
            } else {
                // The mutation lock spans the whole execution, so a nested
                // mutation tool on a path this call chain already locks must
                // NOT re-acquire (it would self-deadlock): skip keys the
                // ancestors genuinely hold. The held set is shared with
                // concurrent nested siblings of this call, so it is only
                // read (snapshotted) here. This call's own keys are marked
                // as held only *after* its queue locks are acquired, in a
                // fresh set published to its descendants alone — a sibling
                // still acquiring (or already holding) its locks can never
                // make another sibling skip the queue and race the file.
                let ancestor_held: std::collections::BTreeSet<String> = context
                    .held_mutation_keys
                    .as_ref()
                    .map(|held| {
                        held.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                    })
                    .unwrap_or_default();
                let pending: Vec<crate::agent::tool_dependencies::ResourceKey> = call
                    .mutation_paths
                    .iter()
                    .filter(|key| !ancestor_held.contains(key.as_str()))
                    .cloned()
                    .collect();
                if pending.is_empty() {
                    let run = run_closure(std::sync::Arc::clone(&args), &context, tool);
                    run_tool_isolated(&tool_name, run()).await
                } else {
                    let mut key_strings: Vec<String> =
                        pending.iter().map(|key| key.as_str().to_owned()).collect();
                    key_strings.sort();
                    key_strings.dedup();
                    let tool = Arc::clone(tool);
                    let args = std::sync::Arc::clone(&args);
                    let mut run_context = context.clone();
                    // Acquire the queue locks first; only a call that holds
                    // them may publish them to its descendants, and the
                    // publication is a new set (ancestor keys + own keys)
                    // that never mutates the sibling-visible ancestor set.
                    queues
                        .run_locked(&pending, async move {
                            let mut held_keys = ancestor_held;
                            held_keys.extend(key_strings);
                            let held = Arc::new(std::sync::Mutex::new(held_keys));
                            run_context.held_mutation_keys = Some(Arc::clone(&held));
                            // `with_held_mutation_keys` returns the rebound
                            // executor by value (a fresh held-key set, shared
                            // `shared`/recorder); re-share it as the context's
                            // single owner so the call's descendants and the
                            // extension-runtime bind all see that one copy.
                            run_context.nested = run_context
                                .nested
                                .take()
                                .map(|executor| Arc::new(executor.with_held_mutation_keys(held)));
                            let run = run_closure(args, &run_context, &tool);
                            run_tool_isolated(&call.tool_name, run()).await
                        })
                        .await
                }
            }
        };
        let duration_ms = Some(u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX));
        // After hook overrides: the rewritten result plus the hook-reported
        // usage (carried on the finalized call; `ToolResult` has no usage slot).
        let (final_result, usage) = match hooks.and_then(|h| h.after.as_ref()) {
            Some(after) => {
                // A panicking host hook must not unwind the batch (and with it
                // the agent loop): keep the real result and log the failure.
                let override_result: AfterToolCallResult =
                    match std::panic::AssertUnwindSafe(after(call, &result, &context))
                        .catch_unwind()
                        .await
                    {
                        Ok(override_result) => override_result,
                        Err(payload) => {
                            tracing::error!(
                                tool = %tool_name,
                                panic = %panic_payload_message(payload.as_ref()),
                                "afterToolCall hook panicked; keeping the tool result"
                            );
                            AfterToolCallResult::default()
                        }
                    };
                crate::agent::tool_execution::apply_after_tool_call_result(result, override_result)
            }
            None => (result, None),
        };

        if final_result.is_error {
            tracing::warn!(
                error = %final_result.content,
                "tool execution returned an error result"
            );
        }
        FinalizedToolCall {
            index: idx,
            call_id,
            tool_name,
            result: final_result,
            usage,
            duration_ms,
        }
    })
}
