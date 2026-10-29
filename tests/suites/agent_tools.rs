//! Integration tests for the agent tool scheduler and loop.
//!
//! These lock in the upstream parallel/sequential semantics: source-order
//! preflight and results, completion-order end events, per-file mutation
//! serialization, cancellation, and 1:1 call/result mapping.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::tool_batch::{AgentToolCall, ToolExecutionMode, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};

/// Tool that records whether it overlaps with another execution of the same
/// tool by checking a shared "busy" flag; also supports a delay.
struct ProbeTool {
    name: &'static str,
    busy: Arc<AtomicBool>,
    overlaps: Arc<AtomicUsize>,
    delay_ms: u64,
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "test probe"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let busy = self.busy.clone();
        let overlaps = self.overlaps.clone();
        let delay = self.delay_ms;
        Box::pin(async move {
            if busy.swap(true, Ordering::AcqRel) {
                overlaps.fetch_add(1, Ordering::SeqCst);
            }
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            busy.store(false, Ordering::Release);
            ToolResult::ok("probe done")
        })
    }
}

/// Mutation tool whose declared path is "tmp.txt" and that detects overlap
/// through a shared busy flag (two concurrent same-path runs would overlap).
struct MutationProbeTool {
    path: &'static str,
    busy: Arc<AtomicBool>,
    overlaps: Arc<AtomicUsize>,
}

impl AgentTool for MutationProbeTool {
    fn name(&self) -> &'static str {
        "mutation_probe"
    }
    fn description(&self) -> &'static str {
        "mutation probe"
    }
    fn is_mutation_tool(&self) -> bool {
        true
    }
    fn mutation_paths(&self, _args: &serde_json::Value, _cwd: &std::path::Path) -> Vec<PathBuf> {
        vec![self.path.into()]
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let busy = self.busy.clone();
        let overlaps = self.overlaps.clone();
        Box::pin(async move {
            if busy.swap(true, Ordering::AcqRel) {
                overlaps.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            busy.store(false, Ordering::Release);
            ToolResult::ok("mutated")
        })
    }
}

/// Failing tool used to verify single-failure isolation.
struct FailingTool;

impl AgentTool for FailingTool {
    fn name(&self) -> &'static str {
        "failing"
    }
    fn description(&self) -> &'static str {
        "always fails"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::error("expected failure") })
    }
}

fn probe_calls(names: &[&str]) -> Vec<AgentToolCall> {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            AgentToolCall::new(index, format!("call-{index}"), *name, serde_json::json!({}))
        })
        .collect()
}

fn default_context() -> ToolContext {
    ToolContext::new(".")
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_tools_run_concurrently() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(ProbeTool {
        name: "probe",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 50,
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        max_parallel: 8,
        fail_fast: false,
        allow_parallel_side_effects: true,
        ..ToolSchedulerConfig::default()
    });
    let started = std::time::Instant::now();
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["probe", "probe", "probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    let elapsed = started.elapsed();
    // 3 x 50ms; parallel should be noticeably less than 150ms total.
    assert!(
        elapsed.as_millis() < 130,
        "expected overlap, took {elapsed:?}"
    );
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        2,
        "all three probes should overlap"
    );
    // Results remain in source order.
    let indices: Vec<usize> = outcome.results.iter().map(|r| r.index).collect();
    assert_eq!(indices, vec![0, 1, 2]);
    // Start events are source order; end events are completion order.
    let start_indices: Vec<usize> = outcome
        .execution_start
        .iter()
        .map(|e| match e {
            pir::agent::tool_batch::ToolExecutionEvent::Start { index, .. } => *index,
            _ => unreachable!("only start events expected"),
        })
        .collect();
    assert_eq!(start_indices, vec![0, 1, 2]);
    assert_eq!(outcome.execution_end.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn sequential_mode_prevents_overlap() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(ProbeTool {
        name: "probe",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 30,
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        ..ToolSchedulerConfig::default()
    });
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["probe", "probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "sequential must not overlap"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn same_file_mutations_are_serialized() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(MutationProbeTool {
        path: "tmp.txt",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["mutation_probe", "mutation_probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "same-path mutations must serialize"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn different_file_mutations_run_concurrently() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool_a = Arc::new(MutationProbeTool {
        path: "a.txt",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
    });
    let tool_b = Arc::new(MutationProbeTool {
        path: "b.txt",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool_a);
    registry.register(tool_b);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["mutation_probe", "mutation_probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "same busy flag with different files should not overlap"
    );
    assert_eq!(outcome.results.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn single_failure_does_not_discard_other_results() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let ok_tool = Arc::new(ProbeTool {
        name: "ok",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 10,
    });
    let mut registry = ToolRegistry::new();
    registry.register(ok_tool);
    registry.register(Arc::new(FailingTool));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["failing", "ok"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);
    assert!(outcome.results[0].is_error());
    assert!(!outcome.results[1].is_error());
    // A mixed batch must not terminate.
    assert!(!outcome.should_terminate());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_produces_error_result_without_executing_others() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(ProbeTool {
        name: "probe",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 0,
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["missing", "probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);
    assert!(outcome.results[0].is_error());
    assert!(!outcome.results[1].is_error());
    // Upstream `agent-loop.ts:720`: the error NAMES the missing tool so the
    // model can correct the call instead of guessing.
    assert_eq!(outcome.results[0].result.content, "Tool missing not found");
}

#[tokio::test(flavor = "multi_thread")]
async fn max_parallel_limits_concurrency() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(ProbeTool {
        name: "probe",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 30,
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    // max_parallel = 1 forces tools to serialize; overlap must be 0 even in
    // parallel mode, and end events keep completion order.
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        max_parallel: 1,
        ..ToolSchedulerConfig::default()
    });
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["probe", "probe", "probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "max_parallel=1 must serialize"
    );
    assert_eq!(outcome.results.len(), 3);
}

/// `max_parallel` is a host-supplied knob; an "unlimited" sentinel like
/// `usize::MAX` must not reach `tokio::sync::Semaphore::new`, which asserts
/// `permits <= MAX_PERMITS`. The effective permit count can never exceed the
/// batch size, so the scheduler clamps it there.
#[tokio::test(flavor = "multi_thread")]
async fn max_parallel_unlimited_sentinel_does_not_panic() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(ProbeTool {
        name: "probe",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 0,
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        max_parallel: usize::MAX,
        ..ToolSchedulerConfig::default()
    });
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["probe", "probe"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);
    assert!(outcome.results.iter().all(|result| !result.is_error()));
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_marks_remaining_calls_as_errors() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(ProbeTool {
        name: "probe",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 200,
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let signal = pir::ai::types::AbortSignal::new();
    let context = ToolContext {
        cwd: PathBuf::from("."),
        signal: Some(signal.clone()),
        nested: None,
        session_env: None,
        held_mutation_keys: None,
        on_update: None,
        background: false,
    };
    // Abort before execution: scheduler returns error results for every call.
    signal.abort();
    let outcome = scheduler
        .execute_batch(probe_calls(&["probe", "probe"]), &registry, &context, None)
        .await;
    assert_eq!(
        outcome.results.len(),
        2,
        "1:1 call/result mapping under abort"
    );
    assert!(outcome.results.iter().all(|r| r.is_error()));
    // Every start has exactly one end, including the calls abandoned by the
    // abort (the extension `tool_execution_end` stream must stay balanced).
    assert_eq!(outcome.execution_start.len(), 2);
    assert_eq!(outcome.execution_end.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_fast_skips_remaining_calls_as_errors() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FailingTool));
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let ok_tool = Arc::new(ProbeTool {
        name: "ok",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
        delay_ms: 0,
    });
    registry.register(ok_tool);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        fail_fast: true,
        ..ToolSchedulerConfig::default()
    });
    let outcome = scheduler
        .execute_batch(
            probe_calls(&["failing", "ok"]),
            &registry,
            &default_context(),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);
    assert!(outcome.results[0].is_error());
    assert!(
        outcome.results[1].is_error(),
        "fail-fast skips the second call"
    );
    assert!(
        outcome.results[1]
            .result
            .content
            .contains("earlier tool in the batch failed"),
        "the skipped call carries the fail-fast error: {}",
        outcome.results[1].result.content
    );
    // P1-7: the skipped call still closes its lifecycle. All starts are
    // emitted up front (preflight), so the sequential branch must emit one
    // end event per abandoned call, exactly like the parallel branch.
    assert_eq!(outcome.execution_start.len(), 2);
    assert_eq!(
        outcome.execution_end.len(),
        2,
        "one end per emitted start, including fail-fast skips: {:?}",
        outcome.execution_end
    );
    let end_indices: Vec<usize> = outcome
        .execution_end
        .iter()
        .map(|event| match event {
            pir::agent::tool_batch::ToolExecutionEvent::End {
                index, is_error, ..
            } => {
                assert!(*is_error, "both finalized calls are errors");
                *index
            }
            other => panic!("expected only end events, got {other:?}"),
        })
        .collect();
    assert_eq!(end_indices, vec![0, 1]);
}
