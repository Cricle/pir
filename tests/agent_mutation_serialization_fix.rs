//! Regression: batch sibling calls must not skip the per-file mutation queue
//! because another sibling is in flight.
//!
//! The agent loop builds one `NestedToolExecutor::root` per batch and the
//! scheduler rebinds it per call through `ToolContext::with_call_scope`. When
//! the rebind reused the root's held-mutation-key set for every call, the
//! first sibling marked its mutation keys into the shared set *before*
//! awaiting the per-file queue, so a later sibling saw the keys as "already
//! held", ran without the queue, and raced the same file (lost updates).
//! Each batch sibling must get an independent set, while nested calls must
//! still inherit — and skip — the keys their ancestor chain holds.
//!
//! The same leak existed one level down: two nested mutations fired
//! *concurrently* by one parent tool (JS `Promise.all` via
//! `ctx.executeTool` / codemode) share the parent chain's set, so the first
//! one's in-flight marker made the second skip the queue. The scheduler now
//! snapshots the ancestor set (read-only), acquires the queue locks first,
//! and only then publishes the acquired keys in a fresh set scoped to the
//! call, visible to its descendants alone — concurrent nested siblings
//! serialize on the per-file queue, while a sequential descendant of a lock
//! holder still skips re-locking (no self-deadlock).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::nested_tool_calls::{NestedEventSink, NestedShared, NestedToolExecutor};
use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};

/// Mutation tool declaring one file; overlap is detected through a shared
/// "busy" flag (two concurrent same-path runs would trip it).
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

/// Build the nested-capable context the agent loop hands a batch: a root
/// executor in the context, rebound per call by the scheduler. Unlike the
/// plain `ToolContext::new`, this exercises the shared-set path.
fn nested_batch_context(registry: Arc<ToolRegistry>, scheduler: ToolScheduler) -> ToolContext {
    let sink: NestedEventSink = Arc::new(|_event| Box::pin(async {}));
    let shared = NestedShared::new(
        registry,
        scheduler,
        None,
        None,
        PathBuf::from("."),
        None,
        false,
        sink,
        None,
        None,
    );
    ToolContext {
        cwd: PathBuf::from("."),
        signal: None,
        nested: Some(Arc::new(NestedToolExecutor::root(shared))),
        session_env: None,
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_siblings_with_nested_context_serialize_same_file_mutations() {
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
    let context = nested_batch_context(Arc::new(registry.clone()), scheduler.clone());
    let calls: Vec<AgentToolCall> = (0..2)
        .map(|index| {
            AgentToolCall::new(
                index,
                format!("call-{index}"),
                "mutation_probe",
                serde_json::json!({}),
            )
        })
        .collect();
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        scheduler.execute_batch(calls, &registry, &context, None),
    )
    .await
    .expect("batch must not deadlock");
    let elapsed = started.elapsed();
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "same-path mutations in one batch must serialize: a sibling in flight \
         must not make another sibling skip the per-file queue"
    );
    assert!(
        elapsed.as_millis() >= 70,
        "two serialized 40ms mutations must take ~80ms, took {elapsed:?}"
    );
}

/// Mutation tool that recurses once through the nested executor onto the
/// same path; the nested call must skip the key its ancestor holds instead
/// of self-deadlocking.
struct RecursingMutationTool {
    recurse: Arc<AtomicBool>,
}

impl AgentTool for RecursingMutationTool {
    fn name(&self) -> &'static str {
        "recursing_mutation"
    }
    fn description(&self) -> &'static str {
        "mutates shared.txt and recurses once"
    }
    fn is_mutation_tool(&self) -> bool {
        true
    }
    fn mutation_paths(&self, _args: &serde_json::Value, cwd: &std::path::Path) -> Vec<PathBuf> {
        vec![cwd.join("shared.txt")]
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let recurse = Arc::clone(&self.recurse);
        let nested = ctx.nested.clone();
        Box::pin(async move {
            if recurse.swap(false, Ordering::SeqCst) {
                let executor = nested.expect("nested executor present");
                let outcome = executor
                    .execute("recursing_mutation", serde_json::json!({}))
                    .await;
                return ToolResult::ok(format!("nested_error={}", outcome.is_error));
            }
            ToolResult::ok("leaf")
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_same_path_mutation_still_skips_ancestor_held_keys() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(RecursingMutationTool {
        recurse: Arc::new(AtomicBool::new(true)),
    }));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let context = nested_batch_context(Arc::new(registry.clone()), scheduler.clone());
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        scheduler.execute_batch(
            vec![AgentToolCall::new(
                0,
                "call-outer",
                "recursing_mutation",
                serde_json::json!({}),
            )],
            &registry,
            &context,
            None,
        ),
    )
    .await
    .expect("nested same-path mutation must not self-deadlock");
    assert_eq!(outcome.results.len(), 1);
    assert!(!outcome.results[0].is_error());
    assert_eq!(outcome.results[0].result.content, "nested_error=false");
}

/// Orchestrator tool that fires two same-path nested mutations concurrently
/// through `ctx.executeTool` (the `Promise.all` / codemode pattern). It is
/// itself not a mutation tool and holds no queue lock, so both children must
/// queue up on the per-file lock; the first child in flight must not make
/// the second skip the queue through the shared parent chain set.
struct ConcurrentNestedMutationTool;

impl AgentTool for ConcurrentNestedMutationTool {
    fn name(&self) -> &'static str {
        "concurrent_nested_mutation"
    }
    fn description(&self) -> &'static str {
        "runs two nested mutation_probe calls concurrently"
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let executor = ctx.nested.clone().expect("nested executor present");
        Box::pin(async move {
            let first = executor.execute("mutation_probe", serde_json::json!({}));
            let second = executor.execute("mutation_probe", serde_json::json!({}));
            let (first, second) = tokio::join!(first, second);
            ToolResult::ok(format!(
                "first_error={} second_error={}",
                first.is_error, second.is_error
            ))
        })
    }
}

/// Regression: concurrent nested siblings on the same file must serialize on
/// the per-file queue. The first sibling's in-flight mark used to land in
/// the parent chain's shared set before it awaited the queue, so the second
/// sibling saw the key as held, skipped `run_locked`, and raced the file
/// (lost update). It is shared by the orchestrator chain only — the skipped
/// call must not happen; both children queue up in turn.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_nested_siblings_on_same_file_serialize() {
    let busy = Arc::new(AtomicBool::new(false));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(MutationProbeTool {
        path: "tmp.txt",
        busy: busy.clone(),
        overlaps: overlaps.clone(),
    }));
    registry.register(Arc::new(ConcurrentNestedMutationTool));
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
    let context = nested_batch_context(Arc::new(registry.clone()), scheduler.clone());
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        scheduler.execute_batch(
            vec![AgentToolCall::new(
                0,
                "call-orchestrator",
                "concurrent_nested_mutation",
                serde_json::json!({}),
            )],
            &registry,
            &context,
            None,
        ),
    )
    .await
    .expect("concurrent nested same-path mutations must serialize without deadlock");
    let elapsed = started.elapsed();
    assert_eq!(outcome.results.len(), 1);
    assert!(!outcome.results[0].is_error());
    assert_eq!(
        outcome.results[0].result.content,
        "first_error=false second_error=false"
    );
    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "a concurrently in-flight nested sibling must not make the other \
         nested call skip the per-file queue and race the same file"
    );
    assert!(
        elapsed.as_millis() >= 70,
        "two serialized 40ms mutations must take ~80ms, took {elapsed:?}"
    );
}
