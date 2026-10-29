//! Background tool calls (`background: true`) — regression pins.
//!
//! The feature detaches a tool call from the turn: the scheduler hands the
//! call's execution future to the session's detach sink, the batch reports it
//! in `ToolBatchOutcome::detached`, the model sees a placeholder result, and
//! the real outcome arrives later as a `backgroundToolResult` transcript
//! entry + a `SessionEvent::BackgroundToolEnd`. Each test below fails if a
//! revert drops one of those pieces.
//!
//! The scheduler-level tests drive `ToolScheduler` directly with probe tools
//! (the `parallel_concurrency.rs` construction pattern); the last test drives
//! the full `AgentSession` facade, because the sink wiring (`build.rs` →
//! `background_sink` → `BackgroundRegistry`) is only reachable there.
//!
//! Step 2 (scoped): mutation tools are ACCEPTED for the background. Their
//! per-file mutation locks are taken inside the call's own execution future
//! (`execute.rs::run_call_body` → `MutationQueues::run_locked`), so a detached
//! mutation call serializes with every other writer of the same file without
//! holding a lock across a turn. The remaining preflight refusal is the tool's
//! own `supports_background() == false` opt-out; the sink's over-limit refusal
//! is unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use pir::agent::agent_loop::{AgentEvent, ProviderRequest};
use pir::agent::nested_tool_calls::{
    NestedEventSink, NestedShared, NestedToolEvent, NestedToolExecutor,
};
use pir::agent::tool_batch::{AgentToolCall, DetachedToolCall, ToolExecutionMode, ToolResult};
use pir::agent::tool_execution::{
    declared_tool_definitions, merge_extension_tools, AgentTool, BoxFuture, ToolContext, ToolHooks,
    ToolRegistry,
};
use pir::agent::tool_scheduler::{DetachSink, ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{
    AgentSession, AgentSessionConfig, EventHandler, SessionEvent, SharedEventStreamFn,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{
    AbortSignal, AssistantMessage, ContentBlock, Model, ModelCost, ModelCostRates, StopReason,
    TerminalReason, TextContent, ThinkingLevel, ToolCall, Usage,
};
use pir::config::settings::FollowUpMode;
use pir::extensions::{ExtensionConfig, ExtensionHandle, ToolResult as ExtensionToolResult};
use pir::session_manager::{CustomMessageEntry, FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A per-test scratch directory: the mutation tests write real files here.
fn fixture_dir(test_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-bg-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

// ---------------------------------------------------------------------------
// Exact scheduler strings (assertion anchors; a revert that rewords any of
// them fails the suite on purpose)
// ---------------------------------------------------------------------------

/// `execute.rs::background_placeholder` — the model-facing answer for an
/// adopted background call.
fn placeholder_text(tool_name: &str, call_id: &str) -> String {
    format!(
        "Tool '{tool_name}' is running in the background (callId: {call_id}). Its result will arrive as a separate message; do not wait for it."
    )
}

/// `execute.rs::background_refused` — the sink refused (background limit).
fn refusal_text(tool_name: &str) -> String {
    format!(
        "Tool '{tool_name}' was not started: the background call limit is reached. Wait for a running background call to finish, or retry without \"background\": true."
    )
}

/// `prepare.rs::prepare_calls` — preflight refusal for a tool that cannot run
/// in the background (`supports_background() == false`; since step 2 mutation
/// tools are admitted and no longer reach this branch).
fn policy_refusal_text(tool_name: &str) -> String {
    format!(
        "Tool '{tool_name}' does not support running in the background; retry the call without \"background\": true."
    )
}

// ---------------------------------------------------------------------------
// Probe tools
// ---------------------------------------------------------------------------

/// A probe tool that records the args it actually received.
///
/// The schema is strict (`additionalProperties: false`) on purpose: if the
/// scheduler ever stopped stripping `background` before validation, a
/// backgrounded probe call would fail with "additional property 'background'
/// is not allowed" instead of running, so the recorded-args pin fails loudly.
/// The counters live behind `Arc`s because `run` returns a `'static` future.
#[derive(Default)]
struct ArgsProbe {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl AgentTool for ArgsProbe {
    fn name(&self) -> &'static str {
        "args-probe"
    }

    fn description(&self) -> &'static str {
        "records the arguments it receives"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } },
            "additionalProperties": false
        }))
    }

    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let seen = Arc::clone(&self.seen);
        let args = args.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(args);
            ToolResult::ok("probe-ok")
        })
    }
}

/// A probe whose run parks until the test releases it: only a batch that does
/// NOT await the detached future can return before the gate opens.
struct GateProbe {
    entered: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Notify>,
}

impl AgentTool for GateProbe {
    fn name(&self) -> &'static str {
        "gate-probe"
    }

    fn description(&self) -> &'static str {
        "parks until the test releases it"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            gate.notified().await;
            ToolResult::ok("gate-open")
        })
    }
}

/// A probe that records whether its OWN context signal was aborted, read once
/// when the tool starts and once after a short sleep (so the run abort lands
/// while the call is in flight).
#[derive(Default)]
struct AbortProbe {
    aborted_at_start: Arc<AtomicBool>,
    aborted_at_end: Arc<AtomicBool>,
}

impl AgentTool for AbortProbe {
    fn name(&self) -> &'static str {
        "abort-probe"
    }

    fn description(&self) -> &'static str {
        "records its own abort-signal state"
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.aborted_at_start
            .store(ctx.is_aborted(), Ordering::SeqCst);
        let signal = ctx.signal.clone();
        let aborted_at_end = Arc::clone(&self.aborted_at_end);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            aborted_at_end.store(
                signal.as_ref().is_some_and(|signal| signal.is_aborted()),
                Ordering::SeqCst,
            );
            ToolResult::ok("detached-ok")
        })
    }
}

/// A non-mutation tool that opts out of backgrounding through
/// `supports_background` (the remaining preflight refusal after step 2
/// admitted mutation tools).
#[derive(Default)]
struct OptOutProbe {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for OptOutProbe {
    fn name(&self) -> &'static str {
        "opt-out-probe"
    }

    fn description(&self) -> &'static str {
        "refuses background execution"
    }

    fn supports_background(&self) -> bool {
        false
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("optout-ok") })
    }
}

/// Adopt every background call (the session sink minus the spawn): the
/// scheduler leaves the future in the `DetachedToolCall`, so tests can inspect
/// it and run the real execution themselves.
fn adopt_all() -> DetachSink {
    Arc::new(|_call: &mut DetachedToolCall| true)
}

// ---------------------------------------------------------------------------
// Scheduler-level tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn background_call_is_stripped_placeholder_and_detached() {
    let probe = Arc::new(ArgsProbe::default());
    let mut registry = ToolRegistry::new();
    registry.register(probe.clone());

    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(adopt_all());
    let calls = vec![AgentToolCall::new(
        0,
        "call-1",
        "args-probe",
        json!({ "value": 7, "background": true }),
    )];
    let mut outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new("."), None)
        .await;

    // The batch answers the provider immediately with the placeholder (the
    // provider requires one tool_result per tool_use) and never asks the model
    // to stop. A failure here means the placeholder was reworded, dropped, or
    // turned into an error/terminate result.
    assert_eq!(outcome.results.len(), 1);
    let finalized = &outcome.results[0];
    assert_eq!(finalized.call_id, "call-1");
    assert_eq!(finalized.index, 0);
    assert!(!finalized.is_error());
    assert_eq!(
        finalized.result.content,
        placeholder_text("args-probe", "call-1")
    );
    assert_eq!(
        finalized.result.details,
        Some(json!({ "background": true, "callId": "call-1", "toolName": "args-probe" }))
    );
    assert!(!outcome.should_terminate());

    // The adopted call is handed over for the session's delivery bookkeeping.
    // A failure here means the scheduler executed the call in the foreground
    // instead of reporting it (or lost the call id/args).
    assert_eq!(outcome.detached.len(), 1);
    let mut detached = outcome.detached.remove(0);
    assert_eq!(detached.call_id, "call-1");
    assert_eq!(detached.tool_name, "args-probe");
    assert_eq!(detached.index, 0);
    assert_eq!(detached.args, json!({ "value": 7 }));
    assert_eq!(detached.parent_call_id, None);
    assert!(
        detached.future.is_some(),
        "an adopted call must carry the execution future until the sink takes it"
    );

    // Run the future the sink left in place: it is the real execution, and the
    // tool body must see the arguments WITHOUT the scheduler key (the strict
    // schema would have rejected a leaked key during preflight).
    let future = detached.take_future().expect("future taken exactly once");
    let result = tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("the detached future must complete once awaited");
    assert_eq!(result.content, "probe-ok");
    assert!(!result.is_error);
    let seen = probe.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the probe must run exactly once");
    assert_eq!(seen[0], json!({ "value": 7 }));
}

/// A host that passes BORROWED hooks (`execute_batch`, not
/// `execute_batch_with_owned_hooks`) cannot hand a detached call's future
/// anything that outlives the turn, so a `background: true` call must be
/// refused loudly instead of silently running in the foreground (where it
/// would block the turn for up to the command timeout while the model was told
/// not to wait).
#[tokio::test]
async fn a_background_call_refuses_when_the_batch_hooks_are_borrowed() {
    let probe = Arc::new(ArgsProbe::default());
    let mut registry = ToolRegistry::new();
    registry.register(probe.clone());

    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(adopt_all());
    let hooks = ToolHooks::default();
    let calls = vec![AgentToolCall::new(
        0,
        "call-borrowed",
        "args-probe",
        json!({ "value": 1, "background": true }),
    )];
    let outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new("."), Some(&hooks))
        .await;

    assert_eq!(outcome.results.len(), 1);
    let finalized = &outcome.results[0];
    assert_eq!(finalized.call_id, "call-borrowed");
    assert!(
        finalized.result.is_error,
        "the call must not report success: {}",
        finalized.result.content
    );
    assert!(
        finalized
            .result
            .content
            .contains("background execution is not available in this host"),
        "the refusal names the reason: {}",
        finalized.result.content
    );
    assert!(
        probe.seen.lock().unwrap().is_empty(),
        "the tool must not run in the foreground after the refusal"
    );
    assert!(
        outcome.detached.is_empty(),
        "nothing may be handed to the detach sink"
    );
}

#[tokio::test]
async fn background_call_does_not_block_the_batch() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(ArgsProbe::default());

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GateProbe {
        entered: Arc::clone(&entered),
        gate: Arc::clone(&gate),
    }));
    registry.register(probe.clone());

    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(adopt_all());
    let calls = vec![
        AgentToolCall::new(0, "call-bg", "gate-probe", json!({ "background": true })),
        AgentToolCall::new(1, "call-plain", "args-probe", json!({ "value": 1 })),
    ];
    // The gate is never opened inside the batch: if a revert made the batch
    // await the backgrounded future, this timeout fires instead.
    let mut outcome = tokio::time::timeout(
        Duration::from_secs(5),
        scheduler.execute_batch(calls, &registry, &ToolContext::new("."), None),
    )
    .await
    .expect("the batch must not await a backgrounded call");

    // Results keep source order: [placeholder, real foreground result].
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(outcome.results[0].call_id, "call-bg");
    assert!(!outcome.results[0].is_error());
    assert_eq!(
        outcome.results[0].result.content,
        placeholder_text("gate-probe", "call-bg")
    );
    assert_eq!(outcome.results[1].call_id, "call-plain");
    assert!(!outcome.results[1].is_error());
    assert_eq!(outcome.results[1].result.content, "probe-ok");

    // The backgrounded tool never ran inside the batch, and the plain call did
    // (with its untouched args).
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    assert_eq!(*probe.seen.lock().unwrap(), vec![json!({ "value": 1 })]);

    // The adopted call is still reported for later execution.
    assert_eq!(outcome.detached.len(), 1);
    let mut detached = outcome.detached.remove(0);
    assert_eq!(detached.call_id, "call-bg");

    // Release the parked call and run its future: the detached future is the
    // real execution and completes once the gate opens.
    let future = detached.take_future().expect("future is present");
    gate.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("the detached call completes once released");
    assert_eq!(result.content, "gate-open");
    assert_eq!(entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn opt_out_tool_refuses_background_with_a_synthesized_error() {
    // The core `edit` tool is registered too: it must NOT be refused any more
    // (step 2 admitted mutation tools) — only the explicit opt-out stays a
    // background policy refusal.
    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let edit = registry
        .get("edit")
        .expect("the core edit tool is registered");
    assert!(
        edit.is_mutation_tool(),
        "the core edit tool must declare the mutation kind"
    );

    // A non-mutation tool that opts out through `supports_background`.
    let opt_out = Arc::new(OptOutProbe::default());
    registry.register(opt_out.clone());

    // A sink is installed and counts its calls: the refusal must not reach it.
    let adopts = Arc::new(AtomicUsize::new(0));
    let adopts_for_sink = Arc::clone(&adopts);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(Arc::new(
        move |_call: &mut DetachedToolCall| {
            adopts_for_sink.fetch_add(1, Ordering::SeqCst);
            true
        },
    ));

    let calls = vec![
        AgentToolCall::new(
            0,
            "call-opt-out",
            "opt-out-probe",
            json!({ "background": true }),
        ),
        // Without the background flag the opting-out tool runs normally: the
        // refusal is a background policy, not a ban on the tool.
        AgentToolCall::new(1, "call-plain", "opt-out-probe", json!({})),
    ];
    let outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new("."), None)
        .await;

    // The background attempt is answered with the exact preflight refusal.
    // A failure here means the policy check was dropped (the call would be
    // detached or run in the foreground) or reworded.
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(outcome.results[0].call_id, "call-opt-out");
    assert!(outcome.results[0].is_error());
    assert_eq!(
        outcome.results[0].result.content,
        policy_refusal_text("opt-out-probe")
    );
    assert_eq!(outcome.results[1].call_id, "call-plain");
    assert!(!outcome.results[1].is_error());
    assert_eq!(outcome.results[1].result.content, "optout-ok");
    assert_eq!(opt_out.runs.load(Ordering::SeqCst), 1);

    assert!(
        outcome.detached.is_empty(),
        "a refused background call must never be detached"
    );
    assert_eq!(
        adopts.load(Ordering::SeqCst),
        0,
        "a refused background call must never reach the sink"
    );
}

/// Step 2: `background: true` on a REAL mutation tool (the core `edit`) is
/// accepted — placeholder answer, one detached entry — and the detached future
/// is the real execution: awaiting it edits the file.
///
/// A failure here means the step-1 mutation refusal came back (no detached
/// entry), the placeholder was reworded, or the detached future no longer
/// performs the edit.
#[tokio::test]
async fn mutation_tool_background_call_edits_the_file_when_the_detached_future_runs() {
    let dir = fixture_dir("mutation-bg-edit");
    std::fs::write(dir.join("target.txt"), "hello world\n").expect("seed file");

    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let edit = registry
        .get("edit")
        .expect("the core edit tool is registered");
    assert!(
        edit.is_mutation_tool(),
        "the core edit tool must declare the mutation kind"
    );

    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(adopt_all());
    let calls = vec![AgentToolCall::new(
        0,
        "call-edit",
        "edit",
        json!({
            "path": "target.txt",
            "edits": [{ "oldText": "hello", "newText": "goodbye" }],
            "background": true
        }),
    )];
    let mut outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new(dir.clone()), None)
        .await;

    assert_eq!(outcome.results.len(), 1);
    assert!(
        !outcome.results[0].is_error(),
        "a mutation background call must not be refused: {}",
        outcome.results[0].result.content
    );
    assert_eq!(
        outcome.results[0].result.content,
        placeholder_text("edit", "call-edit")
    );
    assert_eq!(
        outcome.results[0].result.details,
        Some(json!({ "background": true, "callId": "call-edit", "toolName": "edit" }))
    );
    assert!(!outcome.should_terminate());

    assert_eq!(outcome.detached.len(), 1);
    let mut detached = outcome.detached.remove(0);
    assert_eq!(detached.call_id, "call-edit");
    assert_eq!(detached.tool_name, "edit");
    assert_eq!(
        detached.args,
        json!({
            "path": "target.txt",
            "edits": [{ "oldText": "hello", "newText": "goodbye" }]
        }),
        "the scheduler `background` key must be stripped before the tool sees the args"
    );

    // The detached future performs the edit (the per-file mutation lock runs
    // inside it — see `two_backgrounded_edits_to_the_same_file_serialize`).
    let future = detached.take_future().expect("future taken exactly once");
    let result = tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("the detached future must complete once awaited");
    assert!(
        !result.is_error,
        "the background edit failed: {}",
        result.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("target.txt")).expect("edited file"),
        "goodbye world\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Step 2: two backgrounded edits of the SAME file serialize on the per-file
/// mutation queue.
///
/// Both edits target the same original text, so a serialized pair yields
/// exactly ONE success: the second call re-reads the first call's output and no
/// longer finds its `oldText`. Without the queue both futures would read the
/// seed content and the later write would clobber the earlier one — two
/// successes and a lost update. The loser must therefore fail with the edit
/// tool's not-found error, and the file must carry exactly one whole
/// replacement (never an interleaved/garbled blend).
#[tokio::test]
async fn two_backgrounded_edits_to_the_same_file_serialize() {
    let dir = fixture_dir("mutation-bg-serialize");
    std::fs::write(dir.join("shared.txt"), "target\n").expect("seed file");

    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);

    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(adopt_all());
    let calls = vec![
        AgentToolCall::new(
            0,
            "call-one",
            "edit",
            json!({
                "path": "shared.txt",
                "edits": [{ "oldText": "target", "newText": "from-one" }],
                "background": true
            }),
        ),
        AgentToolCall::new(
            1,
            "call-two",
            "edit",
            json!({
                "path": "shared.txt",
                "edits": [{ "oldText": "target", "newText": "from-two" }],
                "background": true
            }),
        ),
    ];
    let mut outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new(dir.clone()), None)
        .await;

    // Both calls are accepted in one batch and detached: two concurrent
    // futures race for the file's lock.
    assert_eq!(outcome.results.len(), 2);
    for (index, finalized) in outcome.results.iter().enumerate() {
        assert!(
            !finalized.is_error(),
            "edit {index} was refused at preflight: {}",
            finalized.result.content
        );
        assert!(
            finalized
                .result
                .content
                .starts_with("Tool 'edit' is running in the background"),
            "edit {index} was not answered with the placeholder: {}",
            finalized.result.content
        );
    }
    assert_eq!(outcome.detached.len(), 2);
    let first = outcome
        .detached
        .remove(0)
        .take_future()
        .expect("first future");
    let second = outcome
        .detached
        .remove(0)
        .take_future()
        .expect("second future");

    // Drive both concurrently: the queue decides the order, not the caller.
    let (one, two) = tokio::join!(first, second);

    let successes = usize::from(!one.is_error) + usize::from(!two.is_error);
    assert_eq!(
        successes, 1,
        "the two writes were not serialized (both edits applied to the same original text): one={:?} two={:?}",
        one.content, two.content
    );
    let loser = if one.is_error { &one } else { &two };
    assert!(
        loser.content.contains("Could not find"),
        "the second edit must re-read the first edit's output and fail to find its oldText, got: {}",
        loser.content
    );

    let content = std::fs::read_to_string(dir.join("shared.txt")).expect("final file");
    assert!(
        content == "from-one\n" || content == "from-two\n",
        "the file must carry exactly one whole replacement, got {content:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn over_limit_background_call_is_refused() {
    let probe = Arc::new(ArgsProbe::default());
    let mut registry = ToolRegistry::new();
    registry.register(probe.clone());

    // The session sink's limit path: the FIRST adopt succeeds (the registry
    // had capacity), the second is refused because it is full. Sequential
    // execution makes which call hits the sink first deterministic.
    let adopts = Arc::new(AtomicUsize::new(0));
    let adopts_for_sink = Arc::clone(&adopts);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        ..ToolSchedulerConfig::default()
    })
    .with_detach(Arc::new(move |_call: &mut DetachedToolCall| {
        adopts_for_sink.fetch_add(1, Ordering::SeqCst) == 0
    }));

    let calls = vec![
        AgentToolCall::new(
            0,
            "call-1",
            "args-probe",
            json!({ "value": 1, "background": true }),
        ),
        AgentToolCall::new(
            1,
            "call-2",
            "args-probe",
            json!({ "value": 2, "background": true }),
        ),
    ];
    let outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new("."), None)
        .await;

    // First call adopted (placeholder), second refused with the exact
    // limit error. A failure here means the refusal was dropped (the call
    // would be detached) or silently fell back to foreground execution.
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(outcome.results[0].call_id, "call-1");
    assert!(!outcome.results[0].is_error());
    assert_eq!(
        outcome.results[0].result.content,
        placeholder_text("args-probe", "call-1")
    );
    assert_eq!(outcome.results[1].call_id, "call-2");
    assert!(outcome.results[1].is_error());
    assert_eq!(
        outcome.results[1].result.content,
        refusal_text("args-probe")
    );

    assert_eq!(outcome.detached.len(), 1);
    assert_eq!(outcome.detached[0].call_id, "call-1");
    assert_eq!(
        adopts.load(Ordering::SeqCst),
        2,
        "both background requests must reach the sink"
    );
    // Neither call ran: the adopted future was never awaited and the refused
    // one was dropped without executing (no foreground fallback).
    assert!(probe.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn run_abort_does_not_cancel_a_background_call() {
    let probe = Arc::new(AbortProbe::default());
    let mut registry = ToolRegistry::new();
    registry.register(probe.clone());

    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(adopt_all());
    let run_signal = AbortSignal::new();
    let mut context = ToolContext::new(".");
    context.signal = Some(run_signal.clone());

    let calls = vec![AgentToolCall::new(
        0,
        "call-bg",
        "abort-probe",
        json!({ "background": true }),
    )];
    let mut outcome = scheduler
        .execute_batch(calls, &registry, &context, None)
        .await;

    assert_eq!(outcome.detached.len(), 1);
    let mut detached = outcome.detached.remove(0);
    assert_eq!(detached.call_id, "call-bg");
    assert!(
        !detached.abort.is_aborted(),
        "a background call gets its OWN signal, never the run's"
    );

    // Start the detached call on the test runtime, then abort the run's
    // signal (the Esc path) while it is in flight.
    let future = detached.take_future().expect("future is present");
    let task = tokio::spawn(future);
    run_signal.abort();
    assert!(run_signal.is_aborted());
    assert!(
        !detached.abort.is_aborted(),
        "aborting the run must not touch a background call's own signal"
    );

    // The call must survive the run abort and complete normally; a failure
    // here means the detached context was wired to the run signal (the probe
    // would observe an aborted signal) or the future was cancelled.
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the background call must outlive the run abort")
        .expect("the background task must not panic");
    assert!(
        !result.is_error,
        "unexpected abort effect: {}",
        result.content
    );
    assert_eq!(result.content, "detached-ok");
    assert!(
        !probe.aborted_at_start.load(Ordering::SeqCst),
        "the detached context must carry the call's own signal, not the aborted run's"
    );
    assert!(!probe.aborted_at_end.load(Ordering::SeqCst));
}

#[tokio::test]
async fn non_background_batch_is_unchanged() {
    let probe = Arc::new(ArgsProbe::default());
    let mut registry = ToolRegistry::new();
    registry.register(probe.clone());

    // A sink is installed but must never be consulted: no call in this batch
    // asks for the background (`background: false` is not a request).
    let adopts = Arc::new(AtomicUsize::new(0));
    let adopts_for_sink = Arc::clone(&adopts);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(Arc::new(
        move |_call: &mut DetachedToolCall| {
            adopts_for_sink.fetch_add(1, Ordering::SeqCst);
            true
        },
    ));

    let calls = vec![
        AgentToolCall::new(0, "call-1", "args-probe", json!({ "value": 1 })),
        AgentToolCall::new(
            1,
            "call-2",
            "args-probe",
            json!({ "value": 2, "background": false }),
        ),
        AgentToolCall::new(2, "call-3", "args-probe", json!({ "value": 3 })),
    ];
    let outcome = scheduler
        .execute_batch(calls, &registry, &ToolContext::new("."), None)
        .await;

    // A failure here means plain calls started detaching (or the sink is
    // consulted for calls that never asked for the background).
    assert!(
        outcome.detached.is_empty(),
        "no effective background request may detach"
    );
    assert_eq!(
        adopts.load(Ordering::SeqCst),
        0,
        "the detach sink must not be consulted"
    );

    // Every call ran in the foreground, in source order.
    assert_eq!(outcome.results.len(), 3);
    for (expected_index, finalized) in outcome.results.iter().enumerate() {
        assert_eq!(finalized.index, expected_index);
        assert!(!finalized.is_error());
        assert_eq!(finalized.result.content, "probe-ok");
    }
    assert_eq!(outcome.results[1].call_id, "call-2");

    // Each tool saw exactly its own args: `background` is stripped even when
    // it is `false` (only JSON `true` requests background execution). The
    // calls ran in parallel, so their completion order (and thus the recorded
    // order) is arbitrary; compare as a sorted multiset.
    let mut seen = probe.seen.lock().unwrap().clone();
    seen.sort_by_key(|value| value["value"].as_i64().unwrap_or_default());
    assert_eq!(
        seen,
        vec![
            json!({ "value": 1 }),
            json!({ "value": 2 }),
            json!({ "value": 3 })
        ]
    );
}

// ---------------------------------------------------------------------------
// Model-facing declaration
// ---------------------------------------------------------------------------

/// The model can only ask for `background: true` because every declared tool
/// schema carries the scheduler argument. A revert that drops the injection
/// silently switches the whole feature off (the model never learns the
/// argument), and no scheduler-level test would notice — this one does.
#[test]
fn declared_tool_schemas_expose_the_background_argument() {
    let probe = Arc::new(ArgsProbe::default());
    let no_schema = Arc::new(OptOutProbe::default());
    let declared: Vec<Arc<dyn AgentTool>> = vec![probe.clone(), no_schema.clone()];
    let definitions = declared_tool_definitions(&declared, &BTreeMap::new(), &BTreeSet::new());
    let expected = json!({
        "type": "boolean",
        "description": "Run this call in the background: it returns immediately and the real result is delivered as a separate message once it finishes.",
    });
    assert_eq!(definitions.len(), 2);
    assert_eq!(
        definitions[0].parameters["properties"]["background"].clone(),
        expected
    );
    // A tool that declares no schema still gets a usable object schema with
    // the injected argument.
    assert_eq!(
        definitions[1].parameters["properties"]["background"].clone(),
        expected
    );
    assert_eq!(definitions[1].parameters["type"], "object");
    // The injection works on a copy: the tool's own schema stays untouched
    // (the scheduler's strip-before-validation is what keeps the key away from
    // tools with `additionalProperties: false`).
    let own_schema = probe.parameters().expect("the probe declares a schema");
    assert!(own_schema["properties"].get("background").is_none());
}

// ---------------------------------------------------------------------------
// Session-level round trip
// ---------------------------------------------------------------------------

/// The session-level probe: parks until its own signal is aborted, recording
/// entry and cancellation separately so the test can tell "ran and was
/// cancelled" from "ran and finished" (and from "never ran").
#[derive(Default)]
struct SessionProbe {
    entered: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
}

impl AgentTool for SessionProbe {
    fn name(&self) -> &'static str {
        "session-bg-probe"
    }

    fn description(&self) -> &'static str {
        "parks until its own abort signal fires"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } },
            "additionalProperties": false
        }))
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let signal = ctx.signal.clone();
        let aborted = Arc::clone(&self.aborted);
        Box::pin(async move {
            match signal {
                Some(signal) => signal.cancelled().await,
                // A detached call always carries its own signal; a missing one
                // parks instead of finishing, so `aborted` stays 0 and the
                // test fails on the bounded wait below.
                None => std::future::pending::<()>().await,
            }
            aborted.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok("session-aborted")
        })
    }
}

/// The model the scripted session advertises. No provider resolves it: the
/// `event_stream` override replaces the registry stream path entirely.
fn session_model() -> Model {
    Model {
        id: "bg-test-model".into(),
        name: "Background test model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// A scripted provider stream: the first request asks for a backgrounded tool
/// call, every later one ends the run with text — so the run completes on the
/// placeholder result while the detached call is still running.
fn scripted_background_stream() -> SharedEventStreamFn {
    let requests = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_request| {
        let index = requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let message = if index == 0 {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "bg-test-model".into(),
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-bg".into(),
                        name: "session-bg-probe".into(),
                        arguments: BTreeMap::from([
                            ("background".to_owned(), json!(true)),
                            ("value".to_owned(), json!(1)),
                        ]),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    stop_reason: StopReason::ToolUse,
                    timestamp: 1,
                    ..Default::default()
                }
            } else {
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "bg-test-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "dispatched".to_owned(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp: 2,
                    ..Default::default()
                }
            };
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    })
}

#[tokio::test]
async fn session_registry_reflects_and_aborts_background_calls() {
    let probe = Arc::new(SessionProbe::default());
    let mut tools = ToolRegistry::new();
    tools.register(probe.clone());

    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: session_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: Some("background regression test".to_owned()),
        cwd: None,
        event_stream: Some(scripted_background_stream()),
        extensions: None,
        tool_selection: None,
    });

    // The run completes on the placeholder result; the real call keeps running
    // in the session's background registry. A failure here means the session
    // never installed its sink, the loop awaited the detached call, or the
    // placeholder broke the tool_call/tool_result pairing.
    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    session
        .prompt("start", handler)
        .await
        .expect("the prompt must complete on the placeholder result");

    let in_flight = session.background_calls();
    assert_eq!(
        in_flight.len(),
        1,
        "the session registry must own the detached call"
    );
    assert_eq!(in_flight[0].call_id, "call-bg");
    assert_eq!(in_flight[0].tool_name, "session-bg-probe");
    assert_eq!(session.max_background_calls(), 4);

    // The session-switch / teardown path: cancel everything and drain. A
    // failure here means abort_all_background stopped aborting the calls it
    // reports through background_calls.
    assert_eq!(session.abort_all_background(), 1);
    assert!(session.background_calls().is_empty());

    for _ in 0..200 {
        if probe.aborted.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        probe.entered.load(Ordering::SeqCst),
        1,
        "the detached call must actually run"
    );
    assert_eq!(
        probe.aborted.load(Ordering::SeqCst),
        1,
        "abort_all_background must cancel the call's own signal"
    );
}

// ---------------------------------------------------------------------------
// `ctx.executeTool(name, args, { background: true })` — Step 3
// ---------------------------------------------------------------------------

/// The JS probe: `outer` makes three nested calls through `ctx.executeTool` —
/// the first with the documented option `{ background: true }`, the second with
/// the scheduler's `background` argument inside the args payload (the same
/// request through the scheduler's own surface), the third plain foreground —
/// and reports what came back. A `tool_result` handler records every event the
/// runtime fires (on `globalThis`, readable through `eval_json`), so the test
/// can pin the completion-correlation contract.
const NESTED_BACKGROUND_OPTION_TS: &str = r#"
export default function (pi) {
  globalThis.__innerRuns = 0;
  globalThis.__observed = [];
  pi.on("tool_result", (event) => {
    globalThis.__observed.push({
      toolCallId: event.toolCallId,
      toolName: event.toolName,
      parentToolCallId: event.parentToolCallId ?? null,
      isError: event.isError === true,
      text: (event.content ?? []).map((block) => block.text ?? "").join(""),
      inputN: event.input?.n ?? null,
      detailsN: event.details?.n ?? null,
      detailsSawBackgroundArg: event.details?.sawBackgroundArg ?? null,
    });
    return undefined;
  });
  pi.registerTool({
    name: "inner",
    description: "inner tool",
    parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    async execute(_toolCallId, params) {
      globalThis.__innerRuns += 1;
      return {
        content: [{ type: "text", text: "inner:" + params.n }],
        details: {
          n: params.n,
          sawBackgroundArg: Object.prototype.hasOwnProperty.call(params, "background"),
        },
      };
    },
  });
  pi.registerTool({
    name: "outer",
    description: "asks ctx.executeTool for the background",
    parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      // The documented option surface. The awaited envelope is the
      // placeholder; the real result arrives as a `tool_result` event.
      const withOptions = await ctx.executeTool("inner", { n: params.n }, { background: true });
      if (withOptions.isError) throw new Error("options call failed");
      // The scheduler argument form, spliced into the nested args by the
      // CALLER (never the prelude's smuggling of the option): the same
      // request through the scheduler's own surface.
      const withArg = await ctx.executeTool("inner", { n: params.n + 1, background: true });
      if (withArg.isError) throw new Error("args call failed");
      // A plain nested call keeps the synchronous behavior, unchanged.
      const foreground = await ctx.executeTool("inner", { n: params.n + 2 });
      if (foreground.isError) throw new Error("foreground call failed");
      const text = (o) => o.result.content.map((c) => c.text).join("");
      return {
        content: [{ type: "text", text: "outer:" + text(withOptions) + "," + text(withArg) + "," + text(foreground) }],
        details: {
          optionCallDurationMs: withOptions.durationMs,
          argCallDurationMs: withArg.durationMs,
          foregroundCallDurationMs: foreground.durationMs,
          optionCallDetails: withOptions.result.details ?? null,
          argCallDetails: withArg.result.details ?? null,
          foregroundCallDetails: foreground.result.details ?? null,
          optionCallText: text(withOptions),
          argCallText: text(withArg),
          foregroundCallText: text(foreground),
          optionCallToolCallId: withOptions.toolCall.id,
          argCallToolCallId: withArg.toolCall.id,
          foregroundCallToolCallId: foreground.toolCall.id,
        },
      };
    },
  });
}
"#;

/// What the test sink recorded about one adopted nested call: the report
/// fields the session's sink would use (`parent_call_id` included) plus the
/// execution future the test drives itself.
struct AdoptedNested {
    call_id: String,
    parent_call_id: Option<String>,
    tool_name: String,
    args: serde_json::Value,
    future: BoxFuture<'static, ToolResult>,
}

/// Reads the probe's module state (`__innerRuns`, `__observed`).
async fn nested_probe_state(handle: &ExtensionHandle) -> serde_json::Value {
    handle
        .eval_json(
            "JSON.stringify({ innerRuns: globalThis.__innerRuns ?? 0, observed: globalThis.__observed ?? [] })",
        )
        .await
        .expect("probe eval")
}

/// Step 3: `ctx.executeTool(name, args, { background: true })` routes the
/// nested call through the scheduler's detach path.
///
/// Pins, in one pass:
/// - the awaited envelope is the PLACEHOLDER (prompt and non-error), while the
///   detached call has NOT run yet;
/// - the sink adopted the call with `parent_call_id` = the CALLER's id (the
///   nested call's own id is the envelope's `toolCall.id`);
/// - the scheduler argument form (`background` in the nested args) detaches
///   too, and the scheduler key never reaches the tool;
/// - the nested `tool_execution_start` event reports the caller's arguments:
///   the `{ background: true }` option never surfaces in its payload;
/// - the completion fires the JS `tool_result` handler with the SAME call id
///   the placeholder envelope carried (the correlation contract) and, for a
///   nested detach, the requesting call's id as `parentToolCallId`; exactly
///   once per nested call — the placeholder itself is not reported;
/// - a subsequent foreground `ctx.executeTool` behaves exactly as before
///   (real result, numeric duration, `parentToolCallId` on its event).
#[tokio::test(flavor = "current_thread")]
async fn ctx_execute_tool_background_option_detaches_the_nested_call() {
    let dir = fixture_dir("execute-tool-background");
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: dir.clone(),
        mode: "interactive".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start");
    let source = dir.join("nested-background.ts");
    std::fs::write(&source, NESTED_BACKGROUND_OPTION_TS).expect("write fixture");
    let loaded = handle
        .load(std::slice::from_ref(&source))
        .await
        .expect("load");
    assert!(
        loaded.errors.is_empty(),
        "extension load errors: {:?}",
        loaded.errors
    );

    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &handle);

    // The scheduler carries a detach sink (the session wiring minus delivery):
    // it records the adopted call and hands the future to the test, which
    // later runs it and mirrors the session's completion delivery.
    let adopted: Arc<Mutex<Vec<AdoptedNested>>> = Arc::new(Mutex::new(Vec::new()));
    let adopted_for_sink = Arc::clone(&adopted);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_detach(Arc::new(
        move |call: &mut DetachedToolCall| {
            let future = call
                .take_future()
                .expect("an adopted call carries its future");
            adopted_for_sink.lock().unwrap().push(AdoptedNested {
                call_id: call.call_id.clone(),
                parent_call_id: call.parent_call_id.clone(),
                tool_name: call.tool_name.clone(),
                args: call.args.clone(),
                future,
            });
            true
        },
    ));
    // Capture the nested `tool_execution_start` payloads: the event must
    // report the caller's arguments, never the scheduler's `background`
    // request (finding 16).
    let starts: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let starts_for_sink = Arc::clone(&starts);
    let sink: NestedEventSink = Arc::new(move |event| {
        if let NestedToolEvent::Start { call_id, args, .. } = event {
            starts_for_sink.lock().unwrap().push((call_id, args));
        }
        Box::pin(async {})
    });
    let shared = NestedShared::new(
        Arc::new(registry),
        scheduler,
        None,
        Some(handle.clone()),
        dir.clone(),
        None,
        false,
        sink,
        None,
        None,
    );
    let executor = NestedToolExecutor::root(Arc::clone(&shared)).for_call("call-parent");
    handle.bind_nested_executor("call-parent", Arc::new(executor));

    let result: ExtensionToolResult = handle
        .call_tool("outer", "call-parent", &json!({ "n": 7 }))
        .await
        .expect("outer executes");
    let text = result
        .content
        .iter()
        .map(|c| c.text.as_str())
        .collect::<String>();

    // The two backgrounded calls answered with the PLACEHOLDER (this is what
    // makes `outer` return promptly) and the plain call returned for real.
    assert_eq!(
        text,
        format!(
            "outer:{},{},inner:9",
            placeholder_text("inner", "call-parent/0"),
            placeholder_text("inner", "call-parent/1")
        ),
        "the option call and the args call must return the placeholder; the foreground call its real result"
    );
    // A failure here means the placeholder was reworded, dropped, or turned
    // into an error; `durationMs: null` is the placeholder marker.
    assert_eq!(
        result.details["optionCallText"],
        json!(placeholder_text("inner", "call-parent/0"))
    );
    assert!(result.details["optionCallDurationMs"].is_null());
    assert!(result.details["argCallDurationMs"].is_null());
    assert_eq!(
        result.details["optionCallToolCallId"],
        json!("call-parent/0")
    );
    assert_eq!(result.details["argCallToolCallId"], json!("call-parent/1"));
    // The placeholder shape the model/JS sees (`background_placeholder`).
    assert_eq!(
        result.details["optionCallDetails"],
        json!({ "background": true, "callId": "call-parent/0", "toolName": "inner" })
    );
    assert_eq!(
        result.details["argCallDetails"],
        json!({ "background": true, "callId": "call-parent/1", "toolName": "inner" })
    );
    // The foreground nested call is untouched: a REAL result with a numeric
    // duration, no background marker, and `background` stripped from its args.
    assert_eq!(result.details["foregroundCallText"], json!("inner:9"));
    assert!(
        result.details["foregroundCallDurationMs"].is_number(),
        "{:?}",
        result.details
    );
    assert!(result.details["foregroundCallDetails"]["background"].is_null());
    assert_eq!(result.details["foregroundCallDetails"]["n"], json!(9));
    assert_eq!(
        result.details["foregroundCallDetails"]["sawBackgroundArg"],
        json!(false)
    );
    assert_eq!(
        result.details["foregroundCallToolCallId"],
        json!("call-parent/2")
    );

    // The sink adopted exactly the two background requests; each reports the
    // REQUESTING call's id as its parent (`parent_call_id` propagation), and
    // the scheduler `background` key is stripped from the report args.
    let pending = {
        let mut guard = adopted.lock().unwrap();
        std::mem::take(&mut *guard)
    };
    assert_eq!(
        pending.len(),
        2,
        "the option call and the args call must reach the session detach sink"
    );
    assert_eq!(pending[0].call_id, "call-parent/0");
    assert_eq!(
        pending[0].parent_call_id.as_deref(),
        Some("call-parent"),
        "the detached nested call must report its CALLER as the parent"
    );
    assert_eq!(pending[0].tool_name, "inner");
    assert_eq!(pending[0].args, json!({ "n": 7 }));
    assert_eq!(pending[1].call_id, "call-parent/1");
    assert_eq!(pending[1].parent_call_id.as_deref(), Some("call-parent"));
    assert_eq!(pending[1].args, json!({ "n": 8 }));

    // The nested `tool_execution_start` payload reports the caller's
    // arguments exactly as supplied: the `ctx.executeTool(..., { background:
    // true })` option is a scheduler request, not an argument, so it must
    // never surface as `args.background` (the recorder above snapshots the
    // same pre-splice payload).
    let starts = {
        let guard = starts.lock().unwrap();
        guard.clone()
    };
    let start_args = |call_id: &str| {
        starts
            .iter()
            .find(|(id, _)| id == call_id)
            .map(|(_, args)| args.clone())
            .unwrap_or_else(|| panic!("no nested start event for {call_id}: {starts:?}"))
    };
    assert_eq!(
        start_args("call-parent/0"),
        json!({ "n": 7 }),
        "the option-form background request must not surface in tool_execution_start.args"
    );
    // The args form's `background` key was passed BY THE CALLER, so the event
    // mirrors the caller's payload (the scheduler strips it before the tool
    // runs, as the details below prove).
    assert_eq!(
        start_args("call-parent/1"),
        json!({ "n": 8, "background": true })
    );
    assert_eq!(start_args("call-parent/2"), json!({ "n": 9 }));

    // Nothing was awaited: only the foreground call has run so far, and the
    // only `tool_result` event is the foreground call's (the placeholders are
    // NOT reported — the completion is the single event per nested call).
    let state = nested_probe_state(&handle).await;
    assert_eq!(
        state["innerRuns"],
        json!(1),
        "the detached calls must not have run inside the caller"
    );
    let observed = state["observed"].as_array().cloned().unwrap_or_default();
    assert_eq!(observed.len(), 1, "observed: {observed:?}");
    assert_eq!(observed[0]["toolCallId"], json!("call-parent/2"));
    assert_eq!(observed[0]["parentToolCallId"], json!("call-parent"));
    assert_eq!(observed[0]["text"], json!("inner:9"));

    // Deliver the completions the way the session's background delivery does
    // (`agent_session/background.rs`): run the detached future, then dispatch
    // the extension `tool_result` notification with the real result AND the
    // stored parent id (the delivery reads it back from the settled registry
    // entry).
    for entry in pending {
        let AdoptedNested {
            call_id,
            parent_call_id,
            tool_name,
            args,
            future,
        } = entry;
        let completed = tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("the detached nested call must complete once awaited");
        assert!(
            !completed.is_error,
            "the detached call failed: {}",
            completed.content
        );
        let mut inputs = std::collections::HashMap::new();
        inputs.insert(call_id.clone(), args);
        let finalized = pir::agent::tool_batch::FinalizedToolCall {
            index: 0,
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            result: completed,
            usage: None,
            duration_ms: Some(1),
        };
        // The completion's correlation ids: `emit_tool_results` reports the
        // detached call's id as `toolCallId` — the same id the placeholder
        // envelope carried — and the requesting call's id as
        // `parentToolCallId`, exactly as the real delivery passes it.
        let merges = pir::agent::tool_execution::emit_tool_results(
            &handle,
            &inputs,
            std::slice::from_ref(&finalized),
            parent_call_id.as_deref(),
        )
        .await;
        assert!(merges.is_empty(), "the probe handler merges nothing");
    }

    // Every nested call has now run, and the completion events carry the
    // placeholder's call id plus the REAL result — the JS-visible completion
    // surface a `pi.on("tool_result", ...)` handler correlates on.
    let state = nested_probe_state(&handle).await;
    assert_eq!(state["innerRuns"], json!(3));
    let observed = state["observed"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        observed.len(),
        3,
        "exactly one tool_result per nested call (completion, not placeholder): {observed:?}"
    );
    let event_for = |call_id: &str| {
        observed
            .iter()
            .find(|event| event["toolCallId"] == json!(call_id))
            .unwrap_or_else(|| panic!("no tool_result event for {call_id}: {observed:?}"))
            .clone()
    };
    let option_event = event_for("call-parent/0");
    assert_eq!(option_event["text"], json!("inner:7"));
    assert_eq!(option_event["isError"], json!(false));
    assert_eq!(option_event["inputN"], json!(7));
    // The real result's details prove the scheduler key never reached the
    // tool; `parentToolCallId` carries the REQUESTING call id — the value the
    // real delivery reads back from its settled registry entry
    // (`background.rs::BackgroundDelivery::finish`).
    assert_eq!(option_event["detailsN"], json!(7));
    assert_eq!(option_event["detailsSawBackgroundArg"], json!(false));
    assert_eq!(option_event["parentToolCallId"], json!("call-parent"));
    let arg_event = event_for("call-parent/1");
    assert_eq!(arg_event["text"], json!("inner:8"));
    assert_eq!(arg_event["inputN"], json!(8));
    assert_eq!(arg_event["detailsSawBackgroundArg"], json!(false));
    assert_eq!(arg_event["parentToolCallId"], json!("call-parent"));

    handle.unbind_nested_executor("call-parent");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The nested background call through the REAL session sink — Step 3 gap
// ---------------------------------------------------------------------------

/// The session-level fixture: the same `ctx.executeTool` round trip as
/// [`NESTED_BACKGROUND_OPTION_TS`] (same tool names, same `tool_result`
/// capture on `globalThis.__observed`), with one addition that makes the real
/// delivery observable deterministically: the two backgrounded nested calls
/// pass `hold: true`, which parks the inner body until the test flips
/// `globalThis.__innerReleased`. While parked, the detached calls cannot
/// complete — so the test can inspect the session's in-flight registry, the
/// still-empty transcript and the still-unreported `tool_result` surface
/// without racing the completion.
const NESTED_BACKGROUND_SESSION_TS: &str = r#"
export default function (pi) {
  globalThis.__innerRuns = 0;
  globalThis.__observed = [];
  globalThis.__innerReleased = false;
  pi.on("tool_result", (event) => {
    globalThis.__observed.push({
      toolCallId: event.toolCallId,
      toolName: event.toolName,
      parentToolCallId: event.parentToolCallId ?? null,
      isError: event.isError === true,
      text: (event.content ?? []).map((block) => block.text ?? "").join(""),
      inputN: event.input?.n ?? null,
      detailsN: event.details?.n ?? null,
      detailsSawBackgroundArg: event.details?.sawBackgroundArg ?? null,
    });
    // Report usage only for the two DETACHED calls: their real result details
    // carry `held: true` (the fixture parks exactly the background calls).
    // This pins the background completion's usage carrier: a detached call
    // has no tool-result message, so the usage must ride the completion event
    // and the persisted `backgroundToolResult` entry details, exactly once
    // (the placeholder never fires this handler).
    if (event.details?.held === true) {
      return { usage: { input: 10, output: 5 } };
    }
    return undefined;
  });
  pi.registerTool({
    name: "inner",
    description: "inner tool",
    parameters: {
      type: "object",
      properties: { n: { type: "number" }, hold: { type: "boolean" } },
      required: ["n"],
    },
    async execute(_toolCallId, params) {
      globalThis.__innerRuns += 1;
      // `hold: true` parks the call until the test releases it.
      while (params.hold === true && globalThis.__innerReleased !== true) {
        await new Promise((resolve) => setTimeout(resolve, 2));
      }
      return {
        content: [{ type: "text", text: "inner:" + params.n }],
        details: {
          n: params.n,
          sawBackgroundArg: Object.prototype.hasOwnProperty.call(params, "background"),
          held: params.hold === true,
        },
      };
    },
  });
  pi.registerTool({
    name: "outer",
    description: "asks ctx.executeTool for the background",
    parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      // The documented option surface; the awaited envelope is the
      // placeholder of the REAL session sink, and both calls park in the
      // background until the test releases them.
      const withOptions = await ctx.executeTool("inner", { n: params.n, hold: true }, { background: true });
      if (withOptions.isError) throw new Error("options call failed");
      // The scheduler argument form: the same request through the scheduler's
      // own surface, spliced into the nested args by the CALLER.
      const withArg = await ctx.executeTool("inner", { n: params.n + 1, hold: true, background: true });
      if (withArg.isError) throw new Error("args call failed");
      // A plain nested call keeps the synchronous behavior, unchanged.
      const foreground = await ctx.executeTool("inner", { n: params.n + 2 });
      if (foreground.isError) throw new Error("foreground call failed");
      const text = (o) => o.result.content.map((c) => c.text).join("");
      return {
        content: [{ type: "text", text: "outer:" + text(withOptions) + "," + text(withArg) + "," + text(foreground) }],
        details: {
          optionCallText: text(withOptions),
          argCallText: text(withArg),
          foregroundCallText: text(foreground),
          optionCallToolCallId: withOptions.toolCall.id,
          argCallToolCallId: withArg.toolCall.id,
          foregroundCallToolCallId: foreground.toolCall.id,
        },
      };
    },
  });
}
"#;

/// The scripted provider for the session round trip: request 0 asks the model
/// for `outer`; request 1 parks on `turn_two_gate` and then ends the run with
/// text; later requests answer immediately. Every request's serialized message
/// list is captured into `requests`, so the test can pin what the model
/// actually saw (the follow-up injection lands in a later request's context).
fn session_background_stream(
    requests: Arc<Mutex<Vec<String>>>,
    turn_two_gate: Arc<tokio::sync::Notify>,
) -> SharedEventStreamFn {
    Arc::new(move |request: ProviderRequest| {
        let requests = Arc::clone(&requests);
        let turn_two_gate = Arc::clone(&turn_two_gate);
        Box::pin(async move {
            let request_index = {
                let mut captured = requests.lock().unwrap();
                captured.push(
                    serde_json::to_string(&request.context.messages)
                        .unwrap_or_else(|_| "<unserializable context>".to_owned()),
                );
                captured.len() - 1
            };
            let content = if request_index == 0 {
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-outer".to_owned(),
                    name: "outer".to_owned(),
                    arguments: BTreeMap::from([("n".to_owned(), json!(7))]),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                })]
            } else {
                if request_index == 1 {
                    // Keep the run streaming (and its follow-up poll pending)
                    // until the test has observed the delivered completion.
                    turn_two_gate.notified().await;
                }
                vec![ContentBlock::Text(TextContent {
                    text: "dispatched".to_owned(),
                    text_signature: None,
                })]
            };
            let message = AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "bg-test-model".into(),
                content,
                stop_reason: if request_index == 0 {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                },
                timestamp: 1,
                ..Default::default()
            };
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    })
}

/// One recorded `tool_result` event, looked up by the call id it reports.
fn observed_call<'a>(observed: &'a [serde_json::Value], call_id: &str) -> &'a serde_json::Value {
    observed
        .iter()
        .find(|event| event["toolCallId"] == json!(call_id))
        .unwrap_or_else(|| panic!("no tool_result event for {call_id}: {observed:?}"))
}

/// How many recorded `tool_result` events report `call_id` — the correlation
/// contract (exactly one per call: the completion, never the placeholder).
fn observed_count(observed: &[serde_json::Value], call_id: &str) -> usize {
    observed
        .iter()
        .filter(|event| event["toolCallId"] == json!(call_id))
        .count()
}

/// The `backgroundToolResult` custom entries the session persisted, in entry
/// order (the real `BackgroundDelivery::finish` persistence surface).
async fn stored_background_results(session: &AgentSession) -> Vec<CustomMessageEntry> {
    let manager = session
        .session_manager()
        .expect("the session under test is store-backed");
    let store = manager.lock().await;
    let entries: Vec<CustomMessageEntry> = store
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage(entry) if entry.custom_type == "backgroundToolResult" => {
                Some(entry)
            }
            _ => None,
        })
        .collect();
    entries
}

/// The fields of one captured `BackgroundToolEnd` event, looked up by the
/// detached call's id.
fn background_end_for(
    events: &[SessionEvent],
    call_id: &str,
) -> (
    String,
    bool,
    Option<u64>,
    String,
    Option<serde_json::Value>,
    Option<String>,
) {
    let event = events
        .iter()
        .find(|event| {
            matches!(
                event,
                SessionEvent::BackgroundToolEnd { call_id: id, .. } if id.as_str() == call_id
            )
        })
        .unwrap_or_else(|| panic!("no BackgroundToolEnd for {call_id}: {events:?}"));
    match event {
        SessionEvent::BackgroundToolEnd {
            tool_name,
            is_error,
            duration_ms,
            output,
            details,
            parent_call_id,
            ..
        } => (
            tool_name.clone(),
            *is_error,
            *duration_ms,
            output.clone(),
            details.clone(),
            parent_call_id.clone(),
        ),
        _ => unreachable!("filtered above"),
    }
}

/// The handler-reported usage the completion event carries for one detached
/// call. Asserts the call has exactly ONE `BackgroundToolEnd` event, so the
/// usage cannot be counted twice through the event surface.
fn background_usage_for(events: &[SessionEvent], call_id: &str) -> Option<Usage> {
    let mut matching = events.iter().filter(|event| {
        matches!(
            event,
            SessionEvent::BackgroundToolEnd { call_id: id, .. } if id.as_str() == call_id
        )
    });
    let event = matching
        .next()
        .unwrap_or_else(|| panic!("no BackgroundToolEnd for {call_id}: {events:?}"));
    assert!(
        matching.next().is_none(),
        "exactly one BackgroundToolEnd per call, got more for {call_id}: {events:?}"
    );
    match event {
        SessionEvent::BackgroundToolEnd { usage, .. } => *usage,
        _ => unreachable!("filtered above"),
    }
}

/// The model-facing frame `BackgroundDelivery::finish` writes into the
/// transcript (`agent_session/background.rs::background_result_frame`).
fn background_frame(call_id: &str, output: &str, duration_ms: u64) -> String {
    format!(
        "<background-tool-result tool=\"inner\" callId=\"{call_id}\" status=\"ok\" durationMs=\"{duration_ms}\">\n{output}\n</background-tool-result>"
    )
}

/// Step 3's explicit coverage gap: the nested
/// `ctx.executeTool("inner", args, { background: true })` call driven through
/// the REAL session sink.
///
/// The Step-3 test above proves the nested route with a TEST sink that mirrors
/// the session delivery by hand. This test drives one real `session.prompt`
/// run instead: the session's own scheduler (built with
/// `background_sink`/`BackgroundDelivery` over its `BackgroundRegistry`) adopts
/// the detached nested calls, and the session's own delivery persists and
/// reports them.
///
/// Binding path (no test-side `bind_nested_executor`): the run loop builds the
/// run's `NestedToolExecutor::root` and hands it to the scheduler
/// (`agent_loop.rs`), the scheduler scopes it per call
/// (`ToolContext::with_call_scope`), and `ExtensionTool::run` binds that
/// scoped executor to the runtime for the duration of the model-issued
/// `outer` call (`tool_execution.rs`). So the JS `ctx.executeTool` round trip
/// resolves against the real loop's executor — the same one whose nested
/// batches the real detach sink adopts.
///
/// Pins, in one run:
/// - while the two detached calls are parked: the session registry reports
///   them (`background_call_count`/`background_calls`), the store holds no
///   `backgroundToolResult`, no `BackgroundToolEnd` fired, and the JS
///   `tool_result` handler saw only the foreground nested call and `outer` —
///   whose result text embeds the exact PLACEHOLDER envelopes the real
///   scheduler answered the two backgrounded calls with;
/// - after the gate opens: the registry drains, `SessionEvent::BackgroundToolEnd`
///   fires once per detached call with the real output/details, the
///   `backgroundToolResult` custom_message lands in the session store with the
///   exact frame (and details), the completion is injected into the
///   follow_up inbox while the run is still streaming, and each nested call
///   produced exactly ONE `tool_result` event — carrying the placeholder's
///   call id (the correlation contract) plus the requesting call's id as
///   `parentToolCallId` (the adoption-time value the registry carried);
///   the handler-reported usage of the two detached calls rides both the
///   event and the persisted entry details, exactly once per call (a
///   detached call has no tool-result message to carry it);
/// - opening the second request lets the injected frames reach the next
///   provider request's context: the completion reached the running model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_background_via_the_real_session_sink_persists_and_reports() {
    let dir = fixture_dir("session-nested-background");
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: dir.clone(),
        mode: "interactive".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start");
    let source = dir.join("nested-background-session.ts");
    std::fs::write(&source, NESTED_BACKGROUND_SESSION_TS).expect("write fixture");
    let loaded = handle
        .load(std::slice::from_ref(&source))
        .await
        .expect("load");
    assert!(
        loaded.errors.is_empty(),
        "extension load errors: {:?}",
        loaded.errors
    );

    // The REAL session: its constructor installs `background_sink` over its
    // `BackgroundRegistry` (build.rs), and the SAME extension handle is both
    // the tool source and the runtime `BackgroundDelivery` dispatches
    // `tool_result` to — so the delivery the test observes below can only come
    // from the session's own sink, never from a test double.
    let storage =
        SessionManager::in_memory(&dir.to_string_lossy(), None).expect("in-memory session");
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let turn_two_gate = Arc::new(tokio::sync::Notify::new());
    let session = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: session_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(storage),
        compaction: None,
        max_turns: 6,
        system_prompt: Some("background regression test".to_owned()),
        cwd: Some(dir.clone()),
        event_stream: Some(session_background_stream(
            Arc::clone(&requests),
            Arc::clone(&turn_two_gate),
        )),
        extensions: Some(handle.clone()),
        tool_selection: None,
    }));
    // Both injected follow-up frames drain in one poll (the completion must
    // reach the running turn, §2.4.2).
    session.set_follow_up_mode(FollowUpMode::All);

    // The host-side listener: subscribes exactly like a real host would.
    let ends: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ends_for_handler = Arc::clone(&ends);
    session.on_session_event(Arc::new(move |event| {
        if matches!(event, SessionEvent::BackgroundToolEnd { .. }) {
            ends_for_handler.lock().unwrap().push(event);
        }
    }));

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    let run = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.prompt("start", handler).await }
    });

    // The loop adopted both nested background calls (real sink), reached its
    // gated second request, and the detached futures are executing their JS
    // bodies (innerRuns: two held background calls + the foreground one).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let in_flight = session.background_call_count() == 2
            && requests.lock().unwrap().len() >= 2
            && nested_probe_state(&handle).await["innerRuns"] == json!(3);
        if in_flight {
            break;
        }
        let request_count = requests.lock().unwrap().len();
        assert!(
            tokio::time::Instant::now() < deadline,
            "the nested calls never reached the in-flight state: in_flight={} requests={} state={}",
            session.background_call_count(),
            request_count,
            nested_probe_state(&handle).await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // --- in-flight: the session registry reflects both calls (real sink). ---
    assert_eq!(
        session.background_call_count(),
        2,
        "the session's real registry must own both detached nested calls"
    );
    let mut in_flight = session.background_calls();
    in_flight.sort_by(|left, right| left.call_id.cmp(&right.call_id));
    let ids: Vec<(&str, &str, Option<&str>)> = in_flight
        .iter()
        .map(|call| {
            (
                call.call_id.as_str(),
                call.tool_name.as_str(),
                call.parent_call_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            ("call-outer/0", "inner", Some("call-outer")),
            ("call-outer/1", "inner", Some("call-outer")),
        ],
        "background_calls() must report the nested ids and their requesting call id"
    );

    // --- in-flight: NOTHING has been delivered yet. ---
    assert!(
        ends.lock().unwrap().is_empty(),
        "BackgroundToolEnd must not fire while the call is parked"
    );
    assert!(
        stored_background_results(&session).await.is_empty(),
        "the transcript must not carry a backgroundToolResult before the call settles"
    );
    let observed = nested_probe_state(&handle).await["observed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        observed.len(),
        2,
        "only the foreground nested call and outer may report while the background calls are parked: {observed:?}"
    );
    assert_eq!(observed_count(&observed, "call-outer/0"), 0);
    assert_eq!(observed_count(&observed, "call-outer/1"), 0);
    // The model-issued `outer` call completed and its result text embeds the
    // PLACEHOLDER envelopes the real scheduler answered the backgrounded
    // nested calls with (durationMs is not reported by the capture; the exact
    // placeholder text is enough to prove the route).
    assert_eq!(
        observed_call(&observed, "call-outer")["text"],
        json!(format!(
            "outer:{},{},inner:9",
            placeholder_text("inner", "call-outer/0"),
            placeholder_text("inner", "call-outer/1")
        )),
        "outer must have awaited the real scheduler's placeholders"
    );
    let foreground = observed_call(&observed, "call-outer/2");
    assert_eq!(foreground["text"], json!("inner:9"));
    assert_eq!(foreground["parentToolCallId"], json!("call-outer"));

    // Release the gated inner bodies: the detached futures settle and the
    // session's OWN delivery (`BackgroundDelivery::finish`) runs, while the
    // run is still streaming at the gated second request. (`eval_json` parses
    // its JS result as JSON text, hence the `JSON.stringify`.)
    let released = handle
        .eval_json("JSON.stringify(globalThis.__innerReleased = true)")
        .await
        .expect("release the gated nested calls");
    assert_eq!(released, json!(true), "the release flag must be set");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let registry_len = session.background_call_count();
        let end_count = ends.lock().unwrap().len();
        let stored = stored_background_results(&session).await.len();
        // Background frames live on their own inbox; they are not user
        // follow-ups.
        let background_follow_up = session.get_background_follow_up_messages().await;
        let user_follow_up = session.get_follow_up_messages().await;
        let observed_all = nested_probe_state(&handle).await["observed"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if registry_len == 0
            && end_count == 2
            && stored == 2
            && background_follow_up.len() == 2
            && user_follow_up.is_empty()
            && observed_all.len() == 4
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the real delivery path never settled: in_flight={registry_len} end_events={end_count} stored={stored} background={background_follow_up:?} user={user_follow_up:?} observed={observed_all:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // --- the registry drained. ---
    assert_eq!(session.background_call_count(), 0);
    assert!(session.background_calls().is_empty());

    // --- SessionEvent::BackgroundToolEnd fired once per detached call. ---
    let events = {
        let guard = ends.lock().unwrap();
        guard.clone()
    };
    assert_eq!(events.len(), 2, "events: {events:?}");
    let (tool_name_0, is_error_0, duration_0, output_0, details_0, parent_0) =
        background_end_for(&events, "call-outer/0");
    assert_eq!(tool_name_0, "inner");
    assert!(!is_error_0, "the background call succeeded");
    assert_eq!(
        parent_0.as_deref(),
        Some("call-outer"),
        "the completion event must carry the adoption-time requesting call id as parent_call_id"
    );
    let duration_0 = duration_0.expect("the completion reports its duration");
    assert_eq!(output_0, "inner:7");
    assert_eq!(
        details_0,
        Some(json!({ "n": 7, "sawBackgroundArg": false, "held": true }))
    );
    let (_, is_error_1, duration_1, output_1, _, parent_1) =
        background_end_for(&events, "call-outer/1");
    assert!(!is_error_1);
    assert_eq!(parent_1.as_deref(), Some("call-outer"));
    let duration_1 = duration_1.expect("the completion reports its duration");
    assert_eq!(output_1, "inner:8");

    // --- the `tool_result` handler-reported usage: a detached call has no
    // tool-result message to attach it to, so the completion event is the
    // live carrier (and the only surface on a store-less host) and the
    // persisted `backgroundToolResult` entry details are the durable one.
    // Each call reports it exactly once: the handler fires once per call
    // (the placeholder is skipped) and `background_usage_for` rejects a
    // second event for the same call. ---
    let expected_usage = Usage {
        input: 10,
        output: 5,
        total_tokens: 15,
        ..Default::default()
    };
    for call_id in ["call-outer/0", "call-outer/1"] {
        assert_eq!(
            background_usage_for(&events, call_id),
            Some(expected_usage),
            "the completion event must carry the handler-reported usage for {call_id}"
        );
    }
    let expected_usage_json = serde_json::to_value(expected_usage).expect("usage serializes");

    // --- the framed `backgroundToolResult` custom message landed in the
    // session store, written by the session's real delivery. ---
    let stored = stored_background_results(&session).await;
    assert_eq!(stored.len(), 2, "stored: {stored:?}");
    let entry_0 = stored
        .iter()
        .find(|entry| {
            entry
                .details
                .as_ref()
                .and_then(|details| details["callId"].as_str())
                == Some("call-outer/0")
        })
        .unwrap_or_else(|| panic!("no backgroundToolResult entry for call-outer/0: {stored:?}"));
    assert_eq!(entry_0.custom_type, "backgroundToolResult");
    assert!(
        !entry_0.display,
        "a background result is transcript context, not a displayed message"
    );
    assert_eq!(
        entry_0.content,
        json!(background_frame("call-outer/0", "inner:7", duration_0)),
        "the stored frame must be exactly the model-facing frame"
    );
    assert_eq!(
        entry_0.details,
        Some(json!({
            "callId": "call-outer/0",
            "toolName": "inner",
            "status": "ok",
            "durationMs": duration_0,
            "details": { "n": 7, "sawBackgroundArg": false, "held": true },
            "usage": expected_usage_json.clone(),
        }))
    );
    let entry_1 = stored
        .iter()
        .find(|entry| {
            entry
                .details
                .as_ref()
                .and_then(|details| details["callId"].as_str())
                == Some("call-outer/1")
        })
        .unwrap_or_else(|| panic!("no backgroundToolResult entry for call-outer/1: {stored:?}"));
    assert_eq!(
        entry_1.content,
        json!(background_frame("call-outer/1", "inner:8", duration_1))
    );
    assert_eq!(
        entry_1
            .details
            .as_ref()
            .and_then(|details| details.get("usage")),
        Some(&expected_usage_json),
        "the durable carrier: the entry details store the handler-reported usage"
    );
    assert_eq!(
        stored
            .iter()
            .filter(|entry| entry
                .details
                .as_ref()
                .is_some_and(|details| details.get("usage").is_some()))
            .count(),
        2,
        "each detached call persists its usage exactly once: {stored:?}"
    );

    // --- the JS `tool_result` handler: exactly ONE event per nested call,
    // with the call id the placeholder envelope carried (the correlation
    // contract), the requesting call id as `parentToolCallId` (the value the
    // session sink registered at adoption), and the REAL result. ---
    let observed = nested_probe_state(&handle).await["observed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(observed.len(), 4, "observed: {observed:?}");
    assert_eq!(
        observed_count(&observed, "call-outer/0"),
        1,
        "the completion is the single tool_result for the detached call: {observed:?}"
    );
    assert_eq!(observed_count(&observed, "call-outer/1"), 1);
    let completed_0 = observed_call(&observed, "call-outer/0");
    assert_eq!(completed_0["text"], json!("inner:7"));
    assert_eq!(completed_0["isError"], json!(false));
    assert_eq!(completed_0["inputN"], json!(7));
    assert_eq!(completed_0["detailsN"], json!(7));
    assert_eq!(
        completed_0["detailsSawBackgroundArg"],
        json!(false),
        "the scheduler key must never reach the tool"
    );
    assert_eq!(
        completed_0["parentToolCallId"],
        json!("call-outer"),
        "the completion must report the requesting call as the parent (the id the placeholder's call was nested under)"
    );
    let completed_1 = observed_call(&observed, "call-outer/1");
    assert_eq!(completed_1["text"], json!("inner:8"));
    assert_eq!(completed_1["isError"], json!(false));
    assert_eq!(completed_1["inputN"], json!(8));
    assert_eq!(completed_1["detailsSawBackgroundArg"], json!(false));
    assert_eq!(completed_1["parentToolCallId"], json!("call-outer"));

    // --- §2.4.2: the streaming completion was injected as a follow-up. ---
    let queued = session.get_background_follow_up_messages().await;
    let mut queued_sorted = queued.clone();
    queued_sorted.sort();
    let mut expected_frames = vec![
        background_frame("call-outer/0", "inner:7", duration_0),
        background_frame("call-outer/1", "inner:8", duration_1),
    ];
    expected_frames.sort();
    assert_eq!(
        queued_sorted, expected_frames,
        "a completion delivered while the run streams must reach the background follow-up queue"
    );

    // Release the second request: the loop drains the follow-ups into the next
    // request, so the model sees both frames in the same run.
    turn_two_gate.notify_one();
    run.await
        .expect("the prompt task must not panic")
        .expect("the prompt run must succeed");

    let requests = requests.lock().unwrap().clone();
    assert!(
        requests.len() >= 3,
        "the injected follow-up must start another request: {requests:?}"
    );
    assert!(
        !requests[0].contains("background-tool-result")
            && !requests[1].contains("background-tool-result"),
        "no request before the delivery may carry a frame"
    );
    assert_eq!(
        requests[2].matches("background-tool-result").count(),
        4,
        "both frames (opening + closing tag each) must reach the next request's context: {}",
        requests[2]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Aborted background calls and the OS resources they own
// (`TOOL-BACKGROUND-SCOPE.md` §2.5 cancellation/lifetime, §7 risk line:
// "does aborting/dropping a detached bash future reliably kill the pooled
// child process?")
// ---------------------------------------------------------------------------
//
// WHAT THE ABORT DOES (and does NOT do) — the wiring these tests exercise:
// `abort_background` / `abort_all_background` / the `Drop` teardown only fire
// the call's OWN `AbortSignal` and remove its registry entry. The detached
// future is NOT dropped and its completion task is NOT cancelled, so whether
// the OS work stops is the tool's business. The signal DOES reach the bash
// tool on this path — verified anchor by anchor:
//   * `execute.rs::build_detached_call` clones the batch context and replaces
//     its signal with a fresh per-call one (`call_context.signal = Some(..)`)
//     and reports the same handle as `DetachedToolCall::abort`;
//   * `background.rs::BackgroundRegistry::try_reserve` stores that handle, and
//     `abort`/`abort_all` fire it (`entry.abort.abort()`);
//   * the bash tool reads it from its context (`bash.rs`: `ctx.signal`) and
//     hands it to the shell pool, whose wait loop polls it
//     (`shell_pool.rs` `PoolWait::Aborted`) and tears the member down
//     (`PoolMember::terminate` → `ProcessTreeKill::terminate`: a Windows Job
//     Object terminate over the member's job, `taskkill /T /F` as the
//     fallback when no job exists, then `start_kill`), releasing the pool slot
//     so the next call respawns a member. The per-call spawn arm (pool off /
//     protocol loss) polls the same signal and kills the same tree.
//   * `AgentSession::abort_bash` is a DIFFERENT path: it aborts the signal of
//     the out-of-loop `run_shell_command`, which kills with `start_kill`
//     only (`agent_session/bash.rs`), not through the pool member teardown.
//
// The tests below therefore measure the OUTCOME at the OS level rather than
// trusting the machinery: the detached command publishes its own OS pids
// (`/proc/<pid>/winpid` on Windows), and the checks query those pids. The
// authoritative verdict is the OS process view (`tasklist` for a Windows pid,
// `kill -0` elsewhere); the git-bash `kill -0` view of the MSYS pids is
// reported alongside for diagnosis, because MSYS owns a separate process table
// that a kernel job-object kill does not necessarily clear at the same time as
// the Windows process.
//
// `edit`/`write`/`read` cooperate differently: they read `ctx.is_aborted()`
// when their BODY starts — which, for a mutation call, is inside
// `MutationQueues::run_locked` (`execute.rs::run_call_body` defers `tool.run`
// into the locked closure). A detached `edit` queued behind a foreground
// mutation therefore sees the abort at lock-grant time and returns "Operation
// aborted" without touching the file; the late result is discarded by the
// settled-`seq` rule (`BackgroundRegistry::settle` returns `None`), so nothing
// is delivered. Pinned by the last test.

/// Hermetic agent-dir pin for the tests that run the real shell path (same
/// shared [`crate::common::AgentDirEnvGuard`] as `tests/suites/shell_tools.rs`):
/// `current_shell_prefs()` reads `agent_dir()`, so an ambient
/// `~/.pi/agent/settings.json` (`shellPath`, `shellCommandPrefix`,
/// `shellPool: false`) would silently change what these tests execute. The
/// pinned empty directory makes the shell preferences fall back to the
/// built-in defaults (pool enabled, no command prefix, platform bash), and
/// the guard's lock excludes every other agent-dir suite in this binary.
async fn hermetic_agent_dir(label: &str) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::scratch(label, None).await
}

/// [`hermetic_agent_dir`] with `settings.json` written into the pinned
/// directory first (e.g. `{"shellPool": false}` to force the bash tool's
/// per-call arm instead of the persistent pool).
async fn hermetic_agent_dir_pinned(
    label: &str,
    settings_json: Option<&str>,
) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::scratch(label, settings_json).await
}

/// Whether a bash is resolvable on this host: without one the probe command
/// cannot run and the test returns early (the `shell_tools.rs` guard — the
/// suite targets the full-fidelity environment).
fn bash_available() -> bool {
    pir::coding_agent::tools::resolve_shell_config(None).is_ok()
}

/// Per-test scratch directory with RAII cleanup: the detached bash probe
/// publishes its OS process ids here (`pid.txt`) and the checks read them back.
struct ProbeFixture {
    dir: std::path::PathBuf,
}

impl ProbeFixture {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-bg-abort-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        Self { dir }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }
}

impl Drop for ProbeFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The session fixture shared by the resource tests: scripted provider (no
/// network), `cwd` = the probe fixture directory, in-memory transcript
/// (`session: None`, so nothing touches the real agent dir).
fn probe_session_config(
    dir: &std::path::Path,
    tools: ToolRegistry,
    event_stream: Option<SharedEventStreamFn>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: session_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 6,
        system_prompt: Some("background abort resource test".to_owned()),
        cwd: Some(dir.to_path_buf()),
        event_stream,
        extensions: None,
        tool_selection: None,
    }
}

/// The real core tools (`bash`, `edit`, …).
fn core_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    registry
}

/// One scripted assistant message.
fn scripted_message(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "bg-test-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

/// One provider tool call block with JSON arguments.
fn tool_call_block(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: args
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// A text-only turn that ends the run.
fn finished_message(text: &str) -> AssistantMessage {
    scripted_message(
        vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        StopReason::Stop,
    )
}

/// A scripted provider stream: request `n` is answered with `turns[n]` (the
/// last turn repeats). Every request's serialized message list is captured
/// into `requests`, so a test can assert what the model actually saw. When
/// `gate` is set, the request with that index waits for the notify before
/// answering — the caller controls when that turn's tool call is issued.
fn scripted_turn_stream(
    turns: Vec<AssistantMessage>,
    requests: Arc<Mutex<Vec<String>>>,
    gate: Option<(usize, Arc<tokio::sync::Notify>)>,
) -> SharedEventStreamFn {
    // `Arc` so each call can move a handle into its own async block (the
    // stream fn is a `Fn`, callable once per provider request).
    let turns = Arc::new(turns);
    Arc::new(move |request: ProviderRequest| {
        let turns = Arc::clone(&turns);
        let requests = Arc::clone(&requests);
        let gate = gate.clone();
        Box::pin(async move {
            let index = {
                let mut captured = requests.lock().unwrap();
                captured.push(
                    serde_json::to_string(&request.context.messages)
                        .unwrap_or_else(|_| "<unserializable context>".to_owned()),
                );
                captured.len() - 1
            };
            if let Some((gated, notify)) = gate.as_ref() {
                if index == *gated {
                    notify.notified().await;
                }
            }
            let message = turns
                .get(index)
                .or_else(|| turns.last())
                .cloned()
                .unwrap_or_else(|| finished_message("done"));
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    })
}

/// The detached bash probe command: publish the command's own OS process ids,
/// then block in `wait` on a 30s `sleep` child. Each line of `pid.txt` is
/// `<msys pid>:<OS pid>` — `$$` is the (persistent or per-call) shell's pid,
/// `${BASHPID:-$$}` the process that actually runs the command and `$!` the
/// forked `sleep`; the OS pid comes from `/proc/<pid>/winpid` (empty on a
/// platform without it, e.g. Linux). Asserting all three gone covers "the
/// command's own process AND the descendant it spawned". The file is published
/// with a rename so a reader never observes a half-written set. (`${BASHPID}`
/// is captured into `$sub` first on purpose: inside a `$( )` substitution it
/// would expand to the substitution's own pid.)
fn bash_pid_probe_command() -> &'static str {
    "sleep 30 &\nsub=${BASHPID:-$$}\n{\necho \"$$:$(cat /proc/$$/winpid 2>/dev/null)\"\necho \"$sub:$(cat /proc/$sub/winpid 2>/dev/null)\"\necho \"$!:$(cat /proc/$!/winpid 2>/dev/null)\"\n} > pid.tmp\nmv -f pid.tmp pid.txt\nwait\n"
}

/// The in-shell observation body shared by [`pid_observe_command`] and the
/// pooled liveness command: read the `<msys pid>:<OS pid>` pairs published in
/// `pid.txt` and fill two views.
///
/// - `os` — the OS process view, the AUTHORITATIVE one:
///   * on a Windows host (where `tasklist` exists) the probe's Windows pid
///     (`/proc/<pid>/winpid`) is queried with `tasklist /FI "PID eq <pid>"`;
///     a missing mapping is a LOUD failure, never a silent degradation to the
///     git-bash view (git-bash mangles a bare `/FI` into a path, hence the
///     `//FI` spelling; the filter keywords are not localized);
///   * elsewhere (no `tasklist`, e.g. Linux), `kill -0` on the shell pid.
/// - `msys` — the git-bash `kill -0` view of the published shell pids, i.e.
///   what a bash-only probe can see. Reported for DIAGNOSIS: MSYS keeps its own
///   process table, and after a kernel job-object kill the table does not
///   necessarily clear in step with the Windows process.
const PID_SCAN_BODY: &str = r#"total=0
os=""
msys=""
while IFS= read -r line; do
  [ -n "$line" ] || continue
  pid=${line%%:*}
  win=${line#*:}
  total=$((total + 1))
  if command -v tasklist >/dev/null 2>&1; then
    if [ -z "$win" ]; then echo MISSING_WINPID; exit 9; fi
    if tasklist //FI "PID eq $win" 2>/dev/null | grep -q " $win "; then os="$os $win"; fi
  else
    if kill -0 "$pid" 2>/dev/null; then os="$os $pid"; fi
  fi
  if kill -0 "$pid" 2>/dev/null; then msys="$msys $pid"; fi
done < pid.txt
if [ "$total" -eq 0 ]; then echo NO_PIDS_PUBLISHED; exit 8; fi
"#;

/// One single-shot observation of the probe's processes (prints
/// [`PID_SCAN_BODY`]'s totals).
fn pid_observe_command() -> String {
    format!(
        "{PID_SCAN_BODY}echo \"TOTAL=$total\"\necho \"OS_ALIVE=$os\"\necho \"MSYS_ALIVE=$msys\"\n"
    )
}

/// The foreground command for the pool-liveness test: wait in-shell (bounded)
/// until the OS process view reports the aborted command's processes gone,
/// then print the marker — so the marker proves the kill happened AND that a
/// fresh pooled member serves the next call. On timeout the surviving pids
/// (both views) are printed and the command exits 7.
fn pool_liveness_probe_command(deadline_secs: u64) -> String {
    format!(
        "deadline=$((SECONDS + {deadline_secs}))\nok=0\nwhile :; do\n{PID_SCAN_BODY}  if [ -z \"$os\" ]; then ok=1; break; fi\n  if [ \"$SECONDS\" -ge \"$deadline\" ]; then break; fi\n  sleep 0.05\ndone\nif [ \"$ok\" = 1 ]; then echo ALL_DEAD; printf POOL_LIVE_MARKER; else echo \"STILL_ALIVE(os:$os msys:$msys)\"; exit 7; fi\n"
    )
}

/// One observation of the probe's processes, from the two views above.
struct PidVerdict {
    /// How many `<pid>:<winpid>` pairs the probe published.
    total: usize,
    /// Still alive per the OS process view (authoritative).
    os_alive: Vec<String>,
    /// Still alive per the git-bash `kill -0` view (diagnosis only).
    msys_alive: Vec<String>,
    /// The observation command's raw output (failure messages).
    raw: String,
}

impl PidVerdict {
    /// Every published process is gone per the OS view.
    fn gone(&self) -> bool {
        self.total > 0 && self.os_alive.is_empty()
    }

    /// Every published process is visible in the OS view.
    fn all_alive(&self) -> bool {
        self.total > 0 && self.os_alive.len() == self.total
    }

    /// Failure-message rendering of both views.
    fn describe(&self) -> String {
        format!(
            "OS view alive {:?} of {} published; git-bash kill -0 view alive {:?}; raw: {}",
            self.os_alive,
            self.total,
            self.msys_alive,
            self.raw.trim()
        )
    }
}

/// Parse [`pid_observe_command`]'s output.
fn pid_verdict(output: &str) -> PidVerdict {
    let mut verdict = PidVerdict {
        total: 0,
        os_alive: Vec::new(),
        msys_alive: Vec::new(),
        raw: output.to_owned(),
    };
    for line in output.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("TOTAL=") {
            verdict.total = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("OS_ALIVE=") {
            verdict.os_alive = value.split_whitespace().map(str::to_owned).collect();
        } else if let Some(value) = line.strip_prefix("MSYS_ALIVE=") {
            verdict.msys_alive = value.split_whitespace().map(str::to_owned).collect();
        }
    }
    verdict
}

/// Poll (bounded) until the probe published its `<pid>:<winpid>` pairs: the
/// file is written by the detached command once it is really running.
async fn wait_for_probe_pids(path: &std::path::Path, deadline: Duration) -> Vec<String> {
    let started = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let pairs: Vec<String> = text
                .lines()
                .filter(|line| {
                    line.split_once(':').is_some_and(|(pid, _)| {
                        !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())
                    })
                })
                .map(|line| line.trim().to_owned())
                .collect();
            if pairs.len() >= 2 {
                return pairs;
            }
        }
        assert!(
            started.elapsed() < deadline,
            "the detached bash never published its pids to {} within {deadline:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Run one [`pid_observe_command`] through the checker session's out-of-loop
/// bash (the per-call spawn path, independent of the pool under test).
async fn observe_probe(checker: &AgentSession) -> PidVerdict {
    let result = checker.execute_bash(&pid_observe_command()).await;
    pid_verdict(&result.output)
}

/// Poll (bounded) until the OS process view reports every probe process alive:
/// the pre-abort self-validation of the observation mechanism (a mismatch here
/// means the probe's pids are not observable, not that a process leaked).
async fn wait_for_probe_alive(checker: &AgentSession, deadline: Duration) -> PidVerdict {
    let started = Instant::now();
    loop {
        let verdict = observe_probe(checker).await;
        if verdict.all_alive() {
            return verdict;
        }
        assert!(
            started.elapsed() < deadline,
            "the OS process view never saw all probe processes alive within {deadline:?}: {}",
            verdict.describe()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll (bounded) until the OS process view reports no surviving probe process.
async fn wait_for_probe_gone(checker: &AgentSession, deadline: Duration) -> PidVerdict {
    let started = Instant::now();
    loop {
        let verdict = observe_probe(checker).await;
        if verdict.gone() {
            return verdict;
        }
        assert!(
            started.elapsed() < deadline,
            "a killed command's processes are still alive after {deadline:?}: {}",
            verdict.describe()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll (bounded) until `counter` reaches `expected`.
async fn wait_for_counter(
    counter: &Arc<AtomicUsize>,
    expected: usize,
    deadline: Duration,
    what: &str,
) {
    let started = Instant::now();
    loop {
        let value = counter.load(Ordering::SeqCst);
        if value >= expected {
            return;
        }
        assert!(
            started.elapsed() < deadline,
            "{what} never reached {expected} within {deadline:?} (it is {value})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll (bounded) until `path` carries `expected`.
async fn wait_for_file_content(path: &std::path::Path, expected: &str, deadline: Duration) {
    let started = Instant::now();
    loop {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if content == expected {
            return;
        }
        assert!(
            started.elapsed() < deadline,
            "{} never became {expected:?} within {deadline:?} (it is {content:?})",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Item 1: `abort_background` on a detached `bash` must kill the child OS
/// process — the command AND the `sleep` it forked — within the bound, and
/// must drain the session registry immediately (the settled `seq` rule).
///
/// The observation mechanism is self-validating: before the abort the OS
/// process view must see every published pid ALIVE, so the post-abort death
/// verdict cannot be a vacuous pass. The authoritative view is the OS pid
/// (`tasklist` on the probe's `/proc/<pid>/winpid` pid on Windows, `kill -0`
/// elsewhere); the git-bash `kill -0` view of the MSYS pids is reported
/// alongside.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_background_kills_the_detached_bash_process() {
    let _agent_dir = hermetic_agent_dir("abort-kill").await;
    if !bash_available() {
        return;
    }
    // The hermetic defaults must route the call through the persistent shell
    // pool (the member teardown is the kill under test).
    assert!(
        pir::config::settings::current_shell_prefs().pool,
        "the hermetic defaults must leave the persistent shell pool enabled"
    );
    let fixture = ProbeFixture::new("abort-kill");
    // A second, store-less session runs the OS-level checks out of the loop
    // (the fresh-spawn bash path), independently of the pool under test.
    let checker = AgentSession::new(probe_session_config(
        &fixture.dir,
        ToolRegistry::new(),
        None,
    ));

    let turns = vec![
        scripted_message(
            vec![tool_call_block(
                "call-bash-probe",
                "bash",
                json!({ "command": bash_pid_probe_command(), "background": true }),
            )],
            StopReason::ToolUse,
        ),
        finished_message("dispatched"),
    ];
    let session = AgentSession::new(probe_session_config(
        &fixture.dir,
        core_registry(),
        Some(scripted_turn_stream(
            turns,
            Arc::new(Mutex::new(Vec::new())),
            None,
        )),
    ));

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    session
        .prompt("start", handler)
        .await
        .expect("the run must complete on the placeholder result");

    // The detached call is in flight and its command is really running: the
    // pid publish only happens inside the command.
    assert_eq!(
        session.background_call_count(),
        1,
        "the session registry must own the detached bash call"
    );
    let in_flight = session.background_calls();
    assert_eq!(in_flight[0].call_id, "call-bash-probe");
    assert_eq!(in_flight[0].tool_name, "bash");
    let pids = wait_for_probe_pids(&fixture.path("pid.txt"), Duration::from_secs(30)).await;

    // Self-validation of the observation mechanism: every published process
    // must be visible in the OS process view before the abort.
    let live = wait_for_probe_alive(&checker, Duration::from_secs(15)).await;
    assert_eq!(
        live.os_alive.len(),
        live.total,
        "every published process must be alive before the abort ({pids:?}): {}",
        live.describe()
    );

    // The abort under test.
    assert!(
        session.abort_background("call-bash-probe"),
        "the in-flight call must be found by its provider call id"
    );
    assert_eq!(
        session.background_call_count(),
        0,
        "an aborted call is deregistered immediately (its late completion delivers nothing)"
    );
    assert!(
        !session.abort_background("call-bash-probe"),
        "aborting a call that already settled is a no-op false"
    );

    // The OS process must be gone within the bound.
    let gone = wait_for_probe_gone(&checker, Duration::from_secs(15)).await;
    assert!(
        gone.gone(),
        "abort_background must kill the detached command's processes {pids:?}: {}",
        gone.describe()
    );
}

/// The settings command timeout must not cap a detached call: `background:
/// true` means "started to outlive the turn", so the 120 s default (or a small
/// configured cap) applies to foreground commands only. An explicit per-call
/// `timeout` still caps a background call (pinned at the tool level in
/// `tests/shell_command_timeout.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_bash_call_is_exempt_from_the_settings_command_timeout() {
    let _agent_dir = hermetic_agent_dir_pinned(
        "background-timeout-exempt",
        Some(r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 300 } }"#),
    )
    .await;
    if !bash_available() {
        return;
    }
    assert_eq!(
        pir::config::settings::current_shell_prefs().effective_command_timeout_ms(),
        300,
        "the pinned settings must supply the small default under test"
    );
    let fixture = ProbeFixture::new("background-timeout-exempt");
    let turns = vec![
        scripted_message(
            vec![tool_call_block(
                "call-bg-slow",
                "bash",
                json!({ "command": "sleep 0.8; echo bg-done", "background": true }),
            )],
            StopReason::ToolUse,
        ),
        finished_message("dispatched"),
    ];
    let session = AgentSession::new(probe_session_config(
        &fixture.dir,
        core_registry(),
        Some(scripted_turn_stream(
            turns,
            Arc::new(Mutex::new(Vec::new())),
            None,
        )),
    ));
    // The host-side listener, exactly like a real host subscribes.
    let ends: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ends_for_handler = Arc::clone(&ends);
    session.on_session_event(Arc::new(move |event| {
        if matches!(event, SessionEvent::BackgroundToolEnd { .. }) {
            ends_for_handler.lock().unwrap().push(event);
        }
    }));

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    session
        .prompt("start", handler)
        .await
        .expect("the run must complete on the placeholder result");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if session.background_call_count() == 0 && !ends.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the detached call never delivered its result"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let events = {
        let guard = ends.lock().unwrap();
        guard.clone()
    };
    assert_eq!(events.len(), 1, "events: {events:?}");
    let (tool_name, is_error, _duration, output, _details, parent) =
        background_end_for(&events, "call-bg-slow");
    assert_eq!(tool_name, "bash");
    assert!(
        parent.is_none(),
        "a model-issued detach has no requesting call"
    );
    assert!(
        !is_error,
        "the 0.8 s command must finish past the 300 ms settings cap: {output}"
    );
    assert!(
        output.contains("bg-done"),
        "the command ran to completion: {output}"
    );
    assert!(
        !output.contains("timed out"),
        "the settings cap must not reach a detached call: {output}"
    );
}

/// Item 2: session teardown (`impl Drop for AgentSession` →
/// `abort_all_background`) must kill a detached bash's OS process within the
/// bound. This is the print-mode / process-exit path, where no turn, host or
/// registry remains to reap the command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_session_kills_the_detached_bash_process() {
    let _agent_dir = hermetic_agent_dir("drop-kill").await;
    if !bash_available() {
        return;
    }
    let fixture = ProbeFixture::new("drop-kill");
    let checker = AgentSession::new(probe_session_config(
        &fixture.dir,
        ToolRegistry::new(),
        None,
    ));

    let turns = vec![
        scripted_message(
            vec![tool_call_block(
                "call-bash-probe",
                "bash",
                json!({ "command": bash_pid_probe_command(), "background": true }),
            )],
            StopReason::ToolUse,
        ),
        finished_message("dispatched"),
    ];
    let session = AgentSession::new(probe_session_config(
        &fixture.dir,
        core_registry(),
        Some(scripted_turn_stream(
            turns,
            Arc::new(Mutex::new(Vec::new())),
            None,
        )),
    ));

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    session
        .prompt("start", handler)
        .await
        .expect("the run must complete on the placeholder result");
    assert_eq!(session.background_call_count(), 1);
    let pids = wait_for_probe_pids(&fixture.path("pid.txt"), Duration::from_secs(30)).await;
    let live = wait_for_probe_alive(&checker, Duration::from_secs(15)).await;
    assert_eq!(
        live.os_alive.len(),
        live.total,
        "every published process must be alive before the drop ({pids:?}): {}",
        live.describe()
    );

    // The teardown path: `Drop` aborts every in-flight background call (the
    // call keeps its own signal; the completion task survives the session but
    // must observe the abort and stop the OS work).
    drop(session);

    let gone = wait_for_probe_gone(&checker, Duration::from_secs(15)).await;
    assert!(
        gone.gone(),
        "dropping the session must kill the detached command's processes {pids:?}: {}",
        gone.describe()
    );
}

/// Item 3: after an aborted detached bash, the shell pool must still work — a
/// subsequent foreground `bash` runs to completion promptly (no wedged slot,
/// no leaked member). The foreground call is issued by the SAME scripted run:
/// its second provider request is gated by the test, which opens it only after
/// the abort, so the foreground call cannot overtake the abort.
///
/// Its command first waits in-shell (bounded) for the aborted command's OS
/// processes to die, then prints the marker: the marker therefore proves both
/// that the killed command is gone and that a fresh member serves the next
/// call through the pool (the pinned defaults keep it enabled). A wedged pool
/// slot would block the foreground call forever, so the run is bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreground_bash_still_runs_after_an_aborted_background_bash() {
    let _agent_dir = hermetic_agent_dir("pool-liveness").await;
    if !bash_available() {
        return;
    }
    assert!(
        pir::config::settings::current_shell_prefs().pool,
        "the hermetic defaults must leave the persistent shell pool enabled"
    );
    let fixture = ProbeFixture::new("pool-liveness");
    let checker = AgentSession::new(probe_session_config(
        &fixture.dir,
        ToolRegistry::new(),
        None,
    ));

    let gate = Arc::new(tokio::sync::Notify::new());
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let foreground_command = pool_liveness_probe_command(15);
    let turns = vec![
        scripted_message(
            vec![tool_call_block(
                "call-bash-bg",
                "bash",
                json!({ "command": bash_pid_probe_command(), "background": true }),
            )],
            StopReason::ToolUse,
        ),
        scripted_message(
            vec![tool_call_block(
                "call-bash-fg",
                "bash",
                json!({ "command": foreground_command }),
            )],
            StopReason::ToolUse,
        ),
        finished_message("done"),
    ];
    let session = Arc::new(AgentSession::new(probe_session_config(
        &fixture.dir,
        core_registry(),
        Some(scripted_turn_stream(
            turns,
            Arc::clone(&requests),
            Some((1, Arc::clone(&gate))),
        )),
    )));

    let run = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
            session.prompt("start", handler).await
        }
    });

    // The detached command is running; the run is parked on the gated second
    // request, so nothing foreground has started yet. The OS process view must
    // already see it (otherwise the marker command below would be chasing a
    // pid the mechanism cannot observe).
    let pids = wait_for_probe_pids(&fixture.path("pid.txt"), Duration::from_secs(30)).await;
    let live = wait_for_probe_alive(&checker, Duration::from_secs(15)).await;
    assert_eq!(live.os_alive.len(), live.total, "{}", live.describe());
    assert_eq!(
        session.background_call_count(),
        1,
        "the detached bash call must be in flight before the abort"
    );
    assert!(session.abort_background("call-bash-bg"));
    assert_eq!(session.background_call_count(), 0);

    // Now let the run issue its foreground bash. A wedged pool slot would
    // block it forever, so the whole run is bounded below.
    gate.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the foreground bash never completed: the pool is wedged after the abort")
        .expect("the prompt task must not panic")
        .expect("the prompt run must succeed");
    assert_eq!(outcome.stop_reason, StopReason::Stop);

    let captured = {
        let guard = requests.lock().unwrap();
        guard.clone()
    };
    let last = captured
        .last()
        .expect("the run issued at least one request");
    assert!(
        last.contains("POOL_LIVE_MARKER"),
        "the foreground bash after the abort must complete through the pool (aborted pids {pids:?}): {last}"
    );
    // The probe's own success evidence: it prints `ALL_DEAD` and exits 0 only
    // once every published pid is gone from the OS view. (Do NOT grep for
    // `STILL_ALIVE` here — the probe's command TEXT contains that literal in
    // its else-branch echo, so a substring check would always trip.)
    assert!(
        last.contains("ALL_DEAD") && last.contains("\"exit_code\":0"),
        "the aborted command's processes were still alive when the foreground call ran: {last}"
    );
}

/// A gated MUTATION probe: declares the per-file mutation kind the real `edit`
/// does (so the scheduler serializes it on the same file queue) and parks its
/// body until the test releases it. `entered` is bumped when the BODY starts —
/// which `run_call_body` does inside `MutationQueues::run_locked`, so
/// `entered == 1` proves the per-file lock is held.
struct HoldLockProbe {
    entered: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Notify>,
}

impl AgentTool for HoldLockProbe {
    fn name(&self) -> &'static str {
        "hold-lock-probe"
    }

    fn description(&self) -> &'static str {
        "holds a per-file mutation lock until released"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }))
    }

    fn is_mutation_tool(&self) -> bool {
        true
    }

    fn mutation_paths(
        &self,
        args: &serde_json::Value,
        cwd: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        vec![cwd.join(
            args.get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(""),
        )]
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let gate = Arc::clone(&self.gate);
        let released = Arc::clone(&self.released);
        Box::pin(async move {
            gate.notified().await;
            released.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok("lock-released")
        })
    }
}

/// The seed content the queued detached `edit` targets; the expected result is
/// [`EDITED_CONTENT`].
const SEED_CONTENT: &str = "seed content\n";
const EDITED_CONTENT: &str = "rewritten content\n";

/// Wait (bounded) for the setup state of the queue investigation: the
/// foreground mutation probe holds the per-file lock (`entered == 1`) and the
/// detached `edit` is registered in the session's background registry.
async fn wait_for_queued_detached_call(
    probe: &HoldLockProbe,
    session: &AgentSession,
    deadline: Duration,
) {
    let started = Instant::now();
    loop {
        let entered = probe.entered.load(Ordering::SeqCst);
        let in_flight = session.background_call_count();
        if entered == 1 && in_flight == 1 {
            return;
        }
        assert!(
            started.elapsed() < deadline,
            "the setup never reached (lock held + detached call registered) within {deadline:?}: \
             entered={entered} background={in_flight}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Item 4 (abort-cooperation investigation): what does `abort_background` do
/// to a detached NON-bash call? Concretely — a detached `edit` parked on the
/// per-file mutation queue behind a foreground mutation holding the same file.
///
/// FINDING (pinned below): the queued detached edit is CANCELLED — it never
/// writes, and its (late) failure result is discarded. The abort itself only
/// fires the call's own `AbortSignal`; it does not drop the future and does
/// not cancel the completion task. What stops the edit is the tool's own
/// cooperation: `edit` reads `ctx.is_aborted()` when its body starts, and for
/// a mutation call that start happens inside `MutationQueues::run_locked`
/// (`execute.rs::run_call_body` builds the run closure and only invokes it
/// after the locks are granted), so the queued call observes the abort at the
/// moment the lock frees and returns "Operation aborted" without reading or
/// writing the file. Its completion then delivers nothing: the abort already
/// removed the registry entry, so `BackgroundDelivery::finish` →
/// `BackgroundRegistry::settle(seq)` returns `None`.
///
/// Phase 1 is the CONTROL for the identical fixture WITHOUT the abort: the
/// detached edit takes the lock, writes the file and IS delivered — so phase
/// 2's untouched file is the abort's doing, not a broken setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_a_queued_detached_edit_cancels_it_before_it_writes() {
    // --- Phase 1: control (no abort) — the queued detached edit applies. ---
    {
        let fixture = ProbeFixture::new("edit-queued-control");
        let file = fixture.path("shared.txt");
        std::fs::write(&file, SEED_CONTENT).expect("seed file");

        let probe = Arc::new(HoldLockProbe {
            entered: Arc::new(AtomicUsize::new(0)),
            released: Arc::new(AtomicUsize::new(0)),
            gate: Arc::new(tokio::sync::Notify::new()),
        });
        let mut tools = core_registry();
        tools.register(probe.clone());

        let turns = vec![
            scripted_message(
                vec![
                    tool_call_block(
                        "call-hold",
                        "hold-lock-probe",
                        json!({ "path": "shared.txt" }),
                    ),
                    tool_call_block(
                        "call-edit",
                        "edit",
                        json!({
                            "path": "shared.txt",
                            "edits": [{ "oldText": "seed", "newText": "rewritten" }],
                            "background": true
                        }),
                    ),
                ],
                StopReason::ToolUse,
            ),
            finished_message("done"),
        ];
        let session = Arc::new(AgentSession::new(probe_session_config(
            &fixture.dir,
            tools,
            Some(scripted_turn_stream(
                turns,
                Arc::new(Mutex::new(Vec::new())),
                None,
            )),
        )));
        let delivered = Arc::new(AtomicUsize::new(0));
        let delivered_for_handler = Arc::clone(&delivered);
        session.on_session_event(Arc::new(move |event: SessionEvent| {
            if matches!(
                &event,
                SessionEvent::BackgroundToolEnd { call_id, .. } if call_id.as_str() == "call-edit"
            ) {
                delivered_for_handler.fetch_add(1, Ordering::SeqCst);
            }
        }));

        let run = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
                session.prompt("start", handler).await
            }
        });

        // The foreground mutation holds the lock and the detached edit is
        // queued behind it (nothing has touched the file yet).
        wait_for_queued_detached_call(&probe, &session, Duration::from_secs(20)).await;
        assert_eq!(
            std::fs::read_to_string(&file).expect("read file"),
            SEED_CONTENT,
            "the queued detached edit must not run before the lock is released"
        );

        // Release the lock: the queued detached edit takes it and applies.
        probe.gate.notify_one();
        wait_for_file_content(&file, EDITED_CONTENT, Duration::from_secs(20)).await;
        wait_for_counter(
            &delivered,
            1,
            Duration::from_secs(20),
            "the control delivery's BackgroundToolEnd",
        )
        .await;
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the control run must finish")
            .expect("the control prompt task must not panic")
            .expect("the control prompt run must succeed");
        assert_eq!(probe.released.load(Ordering::SeqCst), 1);
        assert_eq!(session.background_call_count(), 0);
    }

    // --- Phase 2: the abort — the same queued detached edit is cancelled. ---
    {
        let fixture = ProbeFixture::new("edit-queued-aborted");
        let file = fixture.path("shared.txt");
        std::fs::write(&file, SEED_CONTENT).expect("seed file");

        let probe = Arc::new(HoldLockProbe {
            entered: Arc::new(AtomicUsize::new(0)),
            released: Arc::new(AtomicUsize::new(0)),
            gate: Arc::new(tokio::sync::Notify::new()),
        });
        let mut tools = core_registry();
        tools.register(probe.clone());

        let turns = vec![
            scripted_message(
                vec![
                    tool_call_block(
                        "call-hold",
                        "hold-lock-probe",
                        json!({ "path": "shared.txt" }),
                    ),
                    tool_call_block(
                        "call-edit",
                        "edit",
                        json!({
                            "path": "shared.txt",
                            "edits": [{ "oldText": "seed", "newText": "rewritten" }],
                            "background": true
                        }),
                    ),
                ],
                StopReason::ToolUse,
            ),
            finished_message("done"),
        ];
        let session = Arc::new(AgentSession::new(probe_session_config(
            &fixture.dir,
            tools,
            Some(scripted_turn_stream(
                turns,
                Arc::new(Mutex::new(Vec::new())),
                None,
            )),
        )));
        let delivered = Arc::new(AtomicUsize::new(0));
        let delivered_for_handler = Arc::clone(&delivered);
        session.on_session_event(Arc::new(move |event: SessionEvent| {
            if matches!(
                &event,
                SessionEvent::BackgroundToolEnd { call_id, .. } if call_id.as_str() == "call-edit"
            ) {
                delivered_for_handler.fetch_add(1, Ordering::SeqCst);
            }
        }));

        let run = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
                session.prompt("start", handler).await
            }
        });

        // Same setup as the control: the lock is held, the edit is queued, the
        // file is untouched.
        wait_for_queued_detached_call(&probe, &session, Duration::from_secs(20)).await;
        assert_eq!(
            std::fs::read_to_string(&file).expect("read file"),
            SEED_CONTENT,
            "the queued detached edit must not run before the lock is released"
        );

        // The abort under test: the call is deregistered (nothing it produces
        // can be delivered) while it sits on the per-file queue.
        assert!(session.abort_background("call-edit"));
        assert_eq!(session.background_call_count(), 0);

        // Release the lock: the queued edit's body now starts and observes the
        // aborted signal (`edit::run`'s entry check) instead of writing.
        probe.gate.notify_one();
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the aborted run must finish")
            .expect("the prompt task must not panic")
            .expect("the prompt run must succeed");
        assert_eq!(
            probe.released.load(Ordering::SeqCst),
            1,
            "the lock holder must have released: the queued edit then had its turn"
        );

        // Bounded negative window: after the run ended (and the lock was
        // released) the file must stay untouched — the aborted edit had every
        // chance to write.
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            assert_eq!(
                std::fs::read_to_string(&file).expect("read file"),
                SEED_CONTENT,
                "an aborted queued detached edit must never write the file"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            delivered.load(Ordering::SeqCst),
            0,
            "an aborted call delivers nothing: no backgroundToolResult entry, no BackgroundToolEnd"
        );
        assert_eq!(session.background_call_count(), 0);
    }
}

/// The bash tool's PER-CALL arm (pool disabled through the pinned settings)
/// must kill the detached command's whole tree on abort, exactly like the
/// pooled path: the per-call spawn must use the real MSYS bash (a
/// Git-for-Windows `bin\bash.exe` launcher shim is not an MSYS process, so the
/// MSYS subtree sweep cannot resolve the root and the forked descendant
/// survives).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_background_kills_the_detached_bash_process_without_the_pool() {
    let _agent_dir =
        hermetic_agent_dir_pinned("abort-kill-nopool", Some(r#"{ "shellPool": false }"#)).await;
    if !bash_available() {
        return;
    }
    // The pinned settings must route this run through the per-call arm: that
    // is the spawn site under test.
    assert!(
        !pir::config::settings::current_shell_prefs().pool,
        "the pinned settings must disable the persistent shell pool"
    );
    let fixture = ProbeFixture::new("abort-kill-nopool");
    let checker = AgentSession::new(probe_session_config(
        &fixture.dir,
        ToolRegistry::new(),
        None,
    ));

    let turns = vec![
        scripted_message(
            vec![tool_call_block(
                "call-bash-probe",
                "bash",
                json!({ "command": bash_pid_probe_command(), "background": true }),
            )],
            StopReason::ToolUse,
        ),
        finished_message("dispatched"),
    ];
    let session = AgentSession::new(probe_session_config(
        &fixture.dir,
        core_registry(),
        Some(scripted_turn_stream(
            turns,
            Arc::new(Mutex::new(Vec::new())),
            None,
        )),
    ));

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    session
        .prompt("start", handler)
        .await
        .expect("the run must complete on the placeholder result");
    assert_eq!(
        session.background_call_count(),
        1,
        "the session registry must own the detached per-call bash"
    );
    let pids = wait_for_probe_pids(&fixture.path("pid.txt"), Duration::from_secs(30)).await;
    let live = wait_for_probe_alive(&checker, Duration::from_secs(15)).await;
    assert_eq!(live.os_alive.len(), live.total, "{}", live.describe());

    assert!(
        session.abort_background("call-bash-probe"),
        "the in-flight call must be found by its provider call id"
    );
    assert_eq!(session.background_call_count(), 0);

    let gone = wait_for_probe_gone(&checker, Duration::from_secs(15)).await;
    assert!(
        gone.gone(),
        "the per-call abort must kill the detached command's processes {pids:?}: {}",
        gone.describe()
    );
}

/// The out-of-loop `!`-command path (`AgentSession::execute_bash` +
/// `abort_bash`) must kill the command's whole tree: it spawns the real MSYS
/// bash and aborts through the same tree kill as the tool, so a forked
/// descendant (`sleep 30 &` inside the command) does not survive the abort.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_bash_kills_the_out_of_loop_command_tree() {
    let _agent_dir = hermetic_agent_dir("abort-bash-tree").await;
    if !bash_available() {
        return;
    }
    let fixture = ProbeFixture::new("abort-bash-tree");
    let session = Arc::new(AgentSession::new(probe_session_config(
        &fixture.dir,
        ToolRegistry::new(),
        None,
    )));

    let run = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.execute_bash(bash_pid_probe_command()).await }
    });

    // The command is really running (it published its own OS pids).
    let pids = wait_for_probe_pids(&fixture.path("pid.txt"), Duration::from_secs(30)).await;
    let live = wait_for_probe_alive(&session, Duration::from_secs(15)).await;
    assert_eq!(
        live.os_alive.len(),
        live.total,
        "every published process must be alive before the abort ({pids:?}): {}",
        live.describe()
    );

    // `abort_bash` cancels every registered out-of-loop bash (upstream
    // `_bashAbortControllers`); the kill must cover the whole tree.
    session.abort_bash();

    let result = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the aborted `!`-command must settle")
        .expect("the execute_bash task must not panic");
    assert!(
        result.cancelled,
        "the aborted command must report cancellation: {result:?}"
    );
    assert_eq!(
        result.exit_code, None,
        "an aborted `!`-command has no exit code (killed, not exited)"
    );

    let gone = wait_for_probe_gone(&session, Duration::from_secs(15)).await;
    assert!(
        gone.gone(),
        "abort_bash must kill the whole command tree {pids:?}: {}",
        gone.describe()
    );
}
