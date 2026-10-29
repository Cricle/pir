//! Parallel scheduler concurrency tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::nested_tool_calls::{NestedEventSink, NestedShared, NestedToolExecutor};
use pir::agent::tool_batch::{AgentToolCall, ToolExecutionEvent, ToolExecutionMode, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};

struct ConcurrencyCounter {
    name: &'static str,
    count: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    /// When set, a call parks until a sibling is running too. The overlap this
    /// test pins is then event-driven instead of relying on a fixed sleep
    /// window that a loaded scheduler could close.
    overlap: Option<Arc<tokio::sync::Barrier>>,
}

impl AgentTool for ConcurrencyCounter {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "concurrency counter"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let count = self.count.clone();
        let peak = self.peak.clone();
        let overlap = self.overlap.clone();
        Box::pin(async move {
            let current = count.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            match overlap {
                Some(overlap) => {
                    overlap.wait().await;
                }
                None => tokio::time::sleep(std::time::Duration::from_millis(30)).await,
            }
            count.fetch_sub(1, Ordering::SeqCst);
            ToolResult::ok("done")
        })
    }
}

fn calls(count: usize) -> Vec<AgentToolCall> {
    (0..count)
        .map(|i| AgentToolCall::new(i, format!("call-{i}"), "concurrent", serde_json::json!({})))
        .collect()
}

fn registry(
    counter: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    overlap: Option<Arc<tokio::sync::Barrier>>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ConcurrencyCounter {
        name: "concurrent",
        count: counter,
        peak,
        overlap,
    }));
    registry
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_tools_overlap_at_capacity() {
    let counter = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        max_parallel: 4,
        ..ToolSchedulerConfig::default()
    });
    // Every call parks on the barrier until a sibling starts, so a peak below
    // 2 means the scheduler serialized the batch (not that the box stalled).
    // The guard turns that regression into a failure instead of a hang.
    let outcome = tokio::time::timeout(
        OUTER_GUARD,
        scheduler.execute_batch(
            calls(8),
            &registry(counter, peak.clone(), Some(barrier)),
            &ToolContext::new("."),
            None,
        ),
    )
    .await
    .expect("a serialized batch parks forever on the overlap barrier");
    assert_eq!(outcome.results.len(), 8);
    assert!(peak.load(Ordering::SeqCst) >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn sequential_mode_peak_is_one() {
    let counter = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        ..ToolSchedulerConfig::default()
    });
    scheduler
        .execute_batch(
            calls(4),
            &registry(counter, peak.clone(), None),
            &ToolContext::new("."),
            None,
        )
        .await;
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Per-call scheduler timeout (CL18 quality audit §2 Q2, item 13)
// ---------------------------------------------------------------------------

/// What a timeout probe does: answer at once, never answer, or answer after a
/// delay that still fits inside the configured ceiling.
#[derive(Clone, Copy)]
enum Probe {
    Fast,
    Hang,
    Slow(std::time::Duration),
}

struct TimeoutProbe {
    name: &'static str,
    behavior: Probe,
}

impl AgentTool for TimeoutProbe {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "timeout probe"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let name = self.name;
        match self.behavior {
            Probe::Fast => Box::pin(async move { ToolResult::ok(format!("{name}-ok")) }),
            Probe::Hang => Box::pin(async move { std::future::pending::<ToolResult>().await }),
            Probe::Slow(delay) => Box::pin(async move {
                tokio::time::sleep(delay).await;
                ToolResult::ok(format!("{name}-ok"))
            }),
        }
    }
}

fn timeout_registry(probes: &[(&'static str, Probe)]) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for (name, behavior) in probes {
        registry.register(Arc::new(TimeoutProbe {
            name,
            behavior: *behavior,
        }));
    }
    registry
}

fn timeout_call(index: usize, name: &str) -> AgentToolCall {
    AgentToolCall::new(index, format!("call-{index}"), name, serde_json::json!({}))
}

/// Short ceiling used by the timeout tests.
const SHORT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);
/// Outer guard: the batch/call must settle long before this, or the test fails
/// instead of hanging the suite.
const OUTER_GUARD: std::time::Duration = std::time::Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn call_timeout_errors_a_hung_tool_and_the_batch_completes() {
    let registry = timeout_registry(&[("hang", Probe::Hang), ("fast", Probe::Fast)]);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        call_timeout: Some(SHORT_TIMEOUT),
        ..ToolSchedulerConfig::default()
    });
    let outcome = tokio::time::timeout(
        OUTER_GUARD,
        scheduler.execute_batch(
            vec![timeout_call(0, "hang"), timeout_call(1, "fast")],
            &registry,
            &ToolContext::new("."),
            None,
        ),
    )
    .await
    .expect("a hung call must not stall the batch when a ceiling is configured");

    assert_eq!(outcome.results.len(), 2, "the whole batch is finalized");
    let hung = &outcome.results[0];
    assert_eq!(hung.tool_name, "hang");
    assert!(hung.is_error());
    assert!(
        hung.duration_ms.is_some(),
        "the timeout reports how long the call ran"
    );
    assert!(
        hung.result.content.contains("hang"),
        "the error names the tool: {}",
        hung.result.content
    );
    assert!(
        hung.result.content.contains("timed out"),
        "the error explains the timeout: {}",
        hung.result.content
    );
    assert!(
        hung.result.content.contains("50ms"),
        "the error reports the limit: {}",
        hung.result.content
    );

    // The sibling finished normally and its result survived the timeout.
    let fast = &outcome.results[1];
    assert!(!fast.is_error());
    assert_eq!(fast.result.content, "fast-ok");

    // Both calls closed their lifecycle; the hung one is an error end.
    assert_eq!(outcome.execution_start.len(), 2);
    assert_eq!(outcome.execution_end.len(), 2);
    assert!(outcome.execution_end.iter().any(|event| matches!(
        event,
        ToolExecutionEvent::End {
            tool_name,
            is_error: true,
            ..
        } if tool_name == "hang"
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn call_timeout_leaves_a_fast_tool_untouched() {
    let registry = timeout_registry(&[("fast", Probe::Fast)]);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        call_timeout: Some(SHORT_TIMEOUT),
        ..ToolSchedulerConfig::default()
    });
    let outcome = tokio::time::timeout(
        OUTER_GUARD,
        scheduler.execute_batch(
            vec![timeout_call(0, "fast")],
            &registry,
            &ToolContext::new("."),
            None,
        ),
    )
    .await
    .expect("a fast call settles well inside the ceiling");

    assert_eq!(outcome.results.len(), 1);
    assert!(!outcome.results[0].is_error());
    assert_eq!(outcome.results[0].result.content, "fast-ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn call_timeout_none_preserves_the_old_behavior() {
    assert_eq!(
        ToolSchedulerConfig::default().call_timeout,
        None,
        "the scheduler imposes no ceiling by default"
    );
    // Slower than SHORT_TIMEOUT: with the knob unset nothing cuts it off.
    let registry =
        timeout_registry(&[("slow", Probe::Slow(std::time::Duration::from_millis(200)))]);
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        ..ToolSchedulerConfig::default()
    });
    let outcome = tokio::time::timeout(
        OUTER_GUARD,
        scheduler.execute_batch(
            vec![timeout_call(0, "slow")],
            &registry,
            &ToolContext::new("."),
            None,
        ),
    )
    .await
    .expect("no configured ceiling means no scheduler timeout");

    assert_eq!(outcome.results.len(), 1);
    assert!(!outcome.results[0].is_error());
    assert_eq!(outcome.results[0].result.content, "slow-ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn call_timeout_does_not_apply_to_nested_execute() {
    let registry = Arc::new(timeout_registry(&[("hang", Probe::Hang)]));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        call_timeout: Some(SHORT_TIMEOUT),
        ..ToolSchedulerConfig::default()
    });
    let sink: NestedEventSink = Arc::new(|_event| Box::pin(async {}));
    let shared = NestedShared::new(
        registry,
        scheduler,
        None,
        None,
        std::path::PathBuf::from("."),
        None,
        false,
        sink,
        None,
        None,
    );
    let executor = NestedToolExecutor::root(shared).for_call("call-parent");
    // A nested call runs inside the parent tool's execution, so the parent
    // owns its budget: the scheduler ceiling must NOT cut it off. The outer
    // guard firing is the expected outcome.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        executor.execute("hang", serde_json::json!({})),
    )
    .await;
    assert!(
        outcome.is_err(),
        "a nested ctx.executeTool call must not take the scheduler ceiling"
    );
}
