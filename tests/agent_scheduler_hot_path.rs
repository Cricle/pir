//! Regression vectors for the tool scheduler's hot-path semantics:
//! `fail_fast` preflight handling and panic isolation of tool bodies.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pir::agent::tool_batch::{AgentToolCall, ToolExecutionMode, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};

struct ProbeTool {
    ran: Arc<AtomicBool>,
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &str {
        "probe"
    }

    fn description(&self) -> &str {
        "test probe"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let ran = Arc::clone(&self.ran);
        Box::pin(async move {
            ran.store(true, Ordering::SeqCst);
            ToolResult::ok("probe done")
        })
    }
}

struct PanickingTool;

impl AgentTool for PanickingTool {
    fn name(&self) -> &str {
        "panicking"
    }

    fn description(&self) -> &str {
        "panics inside run"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async move {
            panic!("intentional tool panic");
        })
    }
}

/// A preflight failure (unknown tool) must count as the "earlier tool failed"
/// under `fail_fast`; before the fix only executed failures stopped siblings.
#[tokio::test(flavor = "multi_thread")]
async fn fail_fast_skips_calls_after_a_preflight_failure() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ProbeTool {
        ran: Arc::clone(&ran),
    }));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        fail_fast: true,
        ..ToolSchedulerConfig::default()
    });
    let outcome = scheduler
        .execute_batch(
            vec![
                AgentToolCall::new(0, "call-0", "missing-tool", serde_json::json!({})),
                AgentToolCall::new(1, "call-1", "probe", serde_json::json!({})),
            ],
            &registry,
            &ToolContext::new("."),
            None,
        )
        .await;

    assert_eq!(outcome.results.len(), 2);
    assert!(
        outcome.results[0].is_error(),
        "unknown tool fails preflight"
    );
    assert!(
        outcome.results[1].is_error(),
        "the call after the failed preflight is skipped"
    );
    assert!(
        outcome.results[1]
            .result
            .content
            .contains("earlier tool in the batch failed"),
        "skipped call carries the fail-fast error: {}",
        outcome.results[1].result.content
    );
    assert!(
        !ran.load(Ordering::SeqCst),
        "a skipped call must not execute its tool"
    );
    assert_eq!(outcome.execution_start.len(), 2);
    assert_eq!(outcome.execution_end.len(), 2);
}

/// A preflight failure is isolated when `fail_fast` is off: the other calls
/// still execute. (The fail-fast skip must be gated by `fail_fast`; an
/// ungated check regressed the mixed-batch contract.)
#[tokio::test(flavor = "multi_thread")]
async fn preflight_failure_without_fail_fast_does_not_skip_others() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ProbeTool {
        ran: Arc::clone(&ran),
    }));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let outcome = scheduler
        .execute_batch(
            vec![
                AgentToolCall::new(0, "call-0", "missing-tool", serde_json::json!({})),
                AgentToolCall::new(1, "call-1", "probe", serde_json::json!({})),
            ],
            &registry,
            &ToolContext::new("."),
            None,
        )
        .await;

    assert_eq!(outcome.results.len(), 2);
    assert!(outcome.results[0].is_error());
    assert!(
        !outcome.results[1].is_error(),
        "without fail_fast a sibling of a preflight failure still runs: {}",
        outcome.results[1].result.content
    );
    assert!(ran.load(Ordering::SeqCst));
}

/// A panicking tool body is finalized as an error result; it must not unwind
/// the batch (and with it the agent loop) or poison its sibling.
#[tokio::test(flavor = "multi_thread")]
async fn panicking_tool_is_isolated_as_an_error_result() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PanickingTool));
    registry.register(Arc::new(ProbeTool {
        ran: Arc::clone(&ran),
    }));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        ..ToolSchedulerConfig::default()
    });
    let outcome = scheduler
        .execute_batch(
            vec![
                AgentToolCall::new(0, "call-0", "panicking", serde_json::json!({})),
                AgentToolCall::new(1, "call-1", "probe", serde_json::json!({})),
            ],
            &registry,
            &ToolContext::new("."),
            None,
        )
        .await;

    assert_eq!(outcome.results.len(), 2);
    let panicked = &outcome.results[0];
    assert!(panicked.is_error(), "the panic became an error result");
    assert!(
        panicked.result.content.contains("panicked"),
        "the result explains the panic: {}",
        panicked.result.content
    );
    assert!(
        !outcome.results[1].is_error(),
        "the sibling still completed"
    );
    assert!(ran.load(Ordering::SeqCst));
    assert_eq!(outcome.execution_end.len(), 2);
}
