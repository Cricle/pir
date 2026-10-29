//! Integration tests for canonical resource keys and mutation queues.

use std::path::Path;
use std::sync::Arc;

use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_dependencies::{MutationQueues, ResourceKey};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::ai::types::AbortSignal;
use serde_json::json;

#[test]
fn logical_keys_collapse_redundant_components() {
    let cwd = Path::new("C:/repo");
    let a = ResourceKey::logical(Path::new("src/a.rs"), cwd);
    let b = ResourceKey::logical(Path::new("./src/../src/a.rs"), cwd);
    assert_eq!(a, b);
}

#[test]
fn relative_and_absolute_resolve_to_same_key() {
    let cwd = Path::new("C:/repo");
    let a = ResourceKey::logical(Path::new("src/a.rs"), cwd);
    let b = ResourceKey::logical(Path::new("C:/repo/src/a.rs"), cwd);
    assert_eq!(a, b);
}

#[test]
fn different_paths_have_distinct_keys() {
    let cwd = Path::new("C:/repo");
    let a = ResourceKey::logical(Path::new("a.txt"), cwd);
    let b = ResourceKey::logical(Path::new("b.txt"), cwd);
    assert_ne!(a, b);
}

// ---------------------------------------------------------------------------
// Regression: mutation-queue deadlock hazards (P2-10)
// ---------------------------------------------------------------------------

/// Keys are de-duplicated before acquisition: locking the same key twice in
/// one call used to self-deadlock (the second acquire waits on the first
/// guard of the same task).
#[tokio::test(flavor = "current_thread")]
async fn run_locked_deduplicates_keys() {
    let queues = MutationQueues::default();
    let key = ResourceKey::logical(Path::new("shared.txt"), Path::new("C:/repo"));
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        queues.run_locked(&[key.clone(), key], async { "ran" }),
    )
    .await
    .expect("run_locked must not self-deadlock on duplicate keys");
    assert_eq!(run, "ran");
}

/// A nested mutation tool targeting a path its ancestor call already holds
/// must skip re-locking: the ancestor's guard spans its whole execution, so
/// re-acquiring would self-deadlock (execute.rs holds the guard across
/// `tool.run`).
#[tokio::test(flavor = "current_thread")]
async fn nested_mutation_on_same_path_does_not_self_deadlock() {
    // `mutator` recurses once through the nested executor onto the same
    // canonical path.
    struct Mutator {
        recurse: Arc<std::sync::atomic::AtomicBool>,
    }

    impl AgentTool for Mutator {
        fn name(&self) -> &'static str {
            "mutator"
        }
        fn description(&self) -> &'static str {
            "mutates shared.txt"
        }
        fn is_mutation_tool(&self) -> bool {
            true
        }
        fn mutation_paths(
            &self,
            _args: &serde_json::Value,
            cwd: &std::path::Path,
        ) -> Vec<std::path::PathBuf> {
            vec![cwd.join("shared.txt")]
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            let recurse = Arc::clone(&self.recurse);
            let nested = ctx.nested.clone();
            Box::pin(async move {
                if recurse.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    let executor = nested.expect("nested executor present");
                    let outcome = executor.execute("mutator", json!({})).await;
                    return ToolResult::ok(format!("outer nested_error={}", outcome.is_error));
                }
                ToolResult::ok("leaf")
            })
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Mutator {
        recurse: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }));
    let scheduler = pir::agent::tool_scheduler::ToolScheduler::default();
    // Per-run scratch cwd: a fixed path would be shared by parallel test runs
    // (and survive between them).
    let cwd = std::env::temp_dir().join(format!(
        "pir-mutation-nested-deadlock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&cwd);
    let sink: pir::agent::nested_tool_calls::NestedEventSink =
        Arc::new(|_event| Box::pin(async {}));
    let shared = pir::agent::nested_tool_calls::NestedShared::new(
        Arc::new(registry.clone()),
        scheduler.clone(),
        None,
        None,
        cwd.clone(),
        Some(AbortSignal::new()),
        false,
        sink,
        None,
        None,
    );
    let executor =
        pir::agent::nested_tool_calls::NestedToolExecutor::root(shared).for_call("call-outer");
    let context = ToolContext {
        cwd: cwd.clone(),
        signal: None,
        nested: Some(Arc::new(executor)),
        session_env: None,
        held_mutation_keys: None,
        on_update: None,
        background: false,
    };

    let run = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        scheduler.execute_batch(
            vec![AgentToolCall::new(0, "call-outer", "mutator", json!({}))],
            &registry,
            &context,
            None,
        ),
    )
    .await
    .expect("nested same-path mutation must not self-deadlock");
    assert_eq!(run.results.len(), 1);
    assert!(!run.results[0].is_error());
    assert_eq!(run.results[0].result.content, "outer nested_error=false");
}
