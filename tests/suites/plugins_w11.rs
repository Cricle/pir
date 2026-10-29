//! W11-F: codemode live-worker admission, tool_search tie determinism,
//! subagent resolved agent dir, and native plugin fan-out preservation.
//!
//! Covers, item by item:
//!
//! - **Global live-worker admission** (`MAX_CONCURRENT_CODEMODE_WORKERS = 2`
//!   upstream): two live sandbox runs hold both process-global slots, a third
//!   run waits, and a slot is released on every exit path — normal return,
//!   panic (task unwind) and abort (cancelled future) — through the owned
//!   permit guard.
//! - **`tool_search` ordering determinism**: equal BM25 scores order by tool
//!   name ascending, independent of document insertion order.
//! - **Subagent description**: rendered from the resolved agent directory
//!   (`agent_dir()/agents`, honoring `PIR_AGENT_DIR`), not a hardcoded
//!   `~/.pi/agent/agents`.
//! - **Native plugin fan-out**: host (plugin list) order is preserved, the
//!   native tool call receives the caller's full `ToolContext` and its images
//!   survive, and the shared event-result merge keeps `input` images,
//!   accumulates `before_agent_start` messages, and preserves image blocks in
//!   a `tool_result` content replacement.
//!
//! Thread-mode fallback (verified by reading, not simulated — the
//! `run_in_worker_process` seam is `pub(super)` and the fallback is the
//! behavior of the calls that precede the admission acquire):
//! `src/plugins/codemode_worker.rs:1332` returns `None` for
//! `PIR_CODEMODE_WORKER=off` and `:1337` for a memoized
//! `EXE_UNAVAILABLE` verdict, both BEFORE the acquire at `:1353`; the
//! thread sandbox in `src/plugins/codemode.rs:1778`+ never touches the
//! semaphore. So a run that cannot use the worker process takes no slot and
//! never queues — its pre-admission semantics are unchanged.
//!
//! Registered by the foreman in `tests/g_agent.rs` (`plugins_w11`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
#[cfg(feature = "extensions")]
use std::time::Duration;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::types::{AbortSignal, ImageContent};
use pir::plugins::{
    AgentPlugin, InputAction, MergedEmit, PluginEvent, PluginEventOutcome, PluginFuture,
    PluginHost, PluginInfo, RegistrationContext,
};

/// Serializes the admission tests: the semaphore is process-global and the
/// tests deliberately fill every slot.
#[cfg(feature = "extensions")]
static ADMISSION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long a wait that MUST NOT complete is allowed to run (short: the
/// assertion is "still queued", not a timing measurement).
#[cfg(feature = "extensions")]
const QUEUED_PROBE: Duration = Duration::from_millis(100);

/// How long a released wait gets to complete (generous bound; the permit is
/// normally granted on the next scheduler turn).
#[cfg(feature = "extensions")]
const RELEASE_BOUND: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// (1) Global live codemode-worker admission
// ---------------------------------------------------------------------------

/// Two live runs fill the cap and a third WAITS; freeing one slot admits it.
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_worker_admission_caps_two_and_admits_the_third_after_release() {
    use pir::plugins::codemode::worker::{
        acquire_live_worker_slot, live_worker_admission_handle, MAX_CONCURRENT_LIVE_WORKERS,
    };

    let _serial = ADMISSION_LOCK.lock().await;
    assert_eq!(
        MAX_CONCURRENT_LIVE_WORKERS, 2,
        "upstream MAX_CONCURRENT_CODEMODE_WORKERS"
    );
    // ONE semaphore for the whole process, not one per host/session: every
    // caller gets the same `Arc`, so two sessions cannot each admit two live
    // workers.
    assert!(
        Arc::ptr_eq(
            &live_worker_admission_handle(),
            &live_worker_admission_handle()
        ),
        "the admission semaphore is process-global"
    );
    // Relative to this test's baseline so a parallel suite holding a slot
    // cannot make the bookkeeping assertions flaky.
    let baseline = live_worker_admission_handle().available_permits();

    let first = tokio::time::timeout(RELEASE_BOUND, acquire_live_worker_slot(None))
        .await
        .expect("slot one is free")
        .expect("slot one");
    let second = tokio::time::timeout(RELEASE_BOUND, acquire_live_worker_slot(None))
        .await
        .expect("slot two is free")
        .expect("slot two");
    assert_eq!(
        live_worker_admission_handle().available_permits(),
        baseline.saturating_sub(2),
        "both slots are held"
    );

    // The third live worker must wait while the two run.
    let mut third = tokio::spawn(async move { acquire_live_worker_slot(None).await });
    let still_queued = tokio::time::timeout(QUEUED_PROBE, &mut third).await;
    assert!(
        still_queued.is_err(),
        "a third live worker must NOT start while both slots are held"
    );

    // Freeing one slot admits it.
    drop(first);
    let admitted = tokio::time::timeout(RELEASE_BOUND, &mut third)
        .await
        .expect("the third worker starts once a slot frees")
        .expect("waiter task joins")
        .expect("the third worker holds a slot");
    drop(second);
    drop(admitted);
    assert_eq!(
        live_worker_admission_handle().available_permits(),
        baseline,
        "every released slot is back"
    );
}

/// A panicking worker releases its slot (the owned permit drops in the
/// unwind).
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_worker_releases_its_admission_slot() {
    use pir::plugins::codemode::worker::{acquire_live_worker_slot, live_worker_admission_handle};

    let _serial = ADMISSION_LOCK.lock().await;
    let baseline = live_worker_admission_handle().available_permits();
    let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
    let panicking = tokio::spawn(async move {
        let _slot = acquire_live_worker_slot(None).await.expect("slot");
        let _ = acquired_tx.send(());
        panic!("simulated codemode worker panic");
    });
    acquired_rx.await.expect("the worker took its slot");
    let joined = panicking.await;
    assert!(
        joined.as_ref().is_err_and(|error| error.is_panic()),
        "the worker task panicked: {joined:?}"
    );

    // Both slots are free again; re-acquiring proves the unwound guard
    // released its permit (a leaked permit would make this wait forever).
    let first = tokio::time::timeout(RELEASE_BOUND, acquire_live_worker_slot(None))
        .await
        .expect("slot after panic")
        .expect("slot after panic");
    let second = tokio::time::timeout(RELEASE_BOUND, acquire_live_worker_slot(None))
        .await
        .expect("second slot after panic")
        .expect("second slot after panic");
    drop(first);
    drop(second);
    assert_eq!(live_worker_admission_handle().available_permits(), baseline);
}

/// An aborted worker releases its slot (cancelling the task drops the owned
/// permit), and an abort while QUEUED never takes a slot.
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_workers_release_their_admission_slots() {
    use pir::plugins::codemode::worker::{acquire_live_worker_slot, live_worker_admission_handle};

    let _serial = ADMISSION_LOCK.lock().await;
    let baseline = live_worker_admission_handle().available_permits();

    // (a) Abort a RUNNING worker: its owned permit drops with the future.
    let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
    let running = tokio::spawn(async move {
        let _slot = acquire_live_worker_slot(None).await.expect("slot");
        let _ = acquired_tx.send(());
        std::future::pending::<()>().await;
    });
    acquired_rx.await.expect("the worker took its slot");
    running.abort();
    let joined = running.await;
    assert!(
        joined.as_ref().is_err_and(|error| error.is_cancelled()),
        "the worker task was aborted: {joined:?}"
    );

    // (b) Abort a QUEUED waiter: the signal cancels the acquire and no slot
    // is taken.
    let first = acquire_live_worker_slot(None).await.expect("slot one");
    let second = acquire_live_worker_slot(None).await.expect("slot two");
    let signal = AbortSignal::new();
    let waiter_signal = signal.clone();
    let waiter = tokio::spawn(async move {
        acquire_live_worker_slot(Some(&waiter_signal))
            .await
            .is_none()
    });
    tokio::task::yield_now().await;
    signal.abort();
    let aborted_while_queued = tokio::time::timeout(RELEASE_BOUND, waiter)
        .await
        .expect("the queued wait is cancelled by the abort signal")
        .expect("waiter task joins");
    assert!(aborted_while_queued, "an aborted waiter takes no slot");
    assert_eq!(
        live_worker_admission_handle().available_permits(),
        baseline.saturating_sub(2),
        "the two held slots are untouched by the aborted waiter"
    );

    drop(first);
    drop(second);
    assert_eq!(live_worker_admission_handle().available_permits(), baseline);
}

/// The two non-panicking exits release too: a worker whose run RETURNS
/// normally and one that returns early through `?`/`return` (the
/// `Option::None` fallback shape) both drop the owned permit.
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_and_early_error_returns_release_their_admission_slots() {
    use pir::plugins::codemode::worker::{acquire_live_worker_slot, live_worker_admission_handle};

    let _serial = ADMISSION_LOCK.lock().await;
    let baseline = live_worker_admission_handle().available_permits();

    // (a) Normal return: the worker body finishes and drops its slot.
    tokio::spawn(async move {
        let _slot = acquire_live_worker_slot(None).await.expect("slot");
    })
    .await
    .expect("the returning worker joins");

    // (b) Early error return: an `?`-style exit out of the worker body after
    // the slot is taken (the fallback/error arms of `run_in_worker_process`
    // all return through the owned guard the same way). `_slot` is a real
    // binding, so it is dropped at scope exit (the error path), not at the
    // end of the statement.
    async fn worker_body_that_fails() -> anyhow::Result<()> {
        let _slot = acquire_live_worker_slot(None)
            .await
            .ok_or_else(|| anyhow::anyhow!("no slot"))?;
        anyhow::bail!("simulated handshake failure after admission")
    }
    let failed = worker_body_that_fails().await;
    assert!(
        failed.is_err(),
        "the simulated worker failed after admission"
    );

    // Both exits freed their slot: re-acquiring twice proves nothing leaked.
    let first = tokio::time::timeout(RELEASE_BOUND, acquire_live_worker_slot(None))
        .await
        .expect("slot after a normal return")
        .expect("slot after a normal return");
    let second = tokio::time::timeout(RELEASE_BOUND, acquire_live_worker_slot(None))
        .await
        .expect("slot after an early error return")
        .expect("slot after an early error return");
    drop(first);
    drop(second);
    assert_eq!(live_worker_admission_handle().available_permits(), baseline);
}

/// A pre-aborted signal takes NO slot at all (the worker is never spawned, so
/// the caller must not fall back to another engine either).
#[cfg(feature = "extensions")]
#[tokio::test(flavor = "current_thread")]
async fn a_pre_aborted_run_never_takes_a_slot() {
    use pir::plugins::codemode::worker::{acquire_live_worker_slot, live_worker_admission_handle};

    let _serial = ADMISSION_LOCK.lock().await;
    let baseline = live_worker_admission_handle().available_permits();
    let signal = AbortSignal::new();
    signal.abort();
    assert!(
        acquire_live_worker_slot(Some(&signal)).await.is_none(),
        "an already-aborted run must not take a slot"
    );
    assert_eq!(
        live_worker_admission_handle().available_permits(),
        baseline,
        "the semaphore is untouched"
    );
}

// ---------------------------------------------------------------------------
// (3) tool_search ordering determinism
// ---------------------------------------------------------------------------

/// Equal BM25 scores order by tool name ascending, independent of the
/// document order (the candidate source is a HashMap-backed registry).
#[cfg(feature = "tool-search")]
#[test]
fn bm25_ties_use_the_documented_name_ascending_key() {
    use pir::plugins::builtin::Bm25Ranker;

    let documents = |order: [&str; 3]| -> Vec<(String, String)> {
        order
            .into_iter()
            .map(|name| (name.to_owned(), "shared searchable text".to_owned()))
            .collect()
    };
    // Identical text => identical scores for all three documents.
    let forward = Bm25Ranker::new(documents(["zeta", "alpha", "middle"]));
    let reversed = Bm25Ranker::new(documents(["middle", "alpha", "zeta"]));

    let forward_ranked = forward.rank("shared", 8);
    let reversed_ranked = reversed.rank("shared", 8);
    assert_eq!(
        forward_ranked, reversed_ranked,
        "the rank order must not depend on document insertion order"
    );
    let names: Vec<&str> = forward_ranked
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["alpha", "middle", "zeta"],
        "ties break on tool name ascending"
    );
    let scores: Vec<f64> = forward_ranked.iter().map(|(_, score)| *score).collect();
    assert!(
        scores
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() < 1e-12),
        "the fixture must actually tie: {scores:?}"
    );
}

/// The primary key stays score DESCENDING: a strictly better match outranks a
/// name that sorts earlier, and only EQUAL scores fall through to the name
/// tie-break. This pins that the determinism fix did not turn the ranker into
/// a plain alphabetical sort.
#[cfg(feature = "tool-search")]
#[test]
fn bm25_keeps_score_descending_as_the_primary_key() {
    use pir::plugins::builtin::Bm25Ranker;

    // `zeta` matches the query three times, `alpha` once: distinct scores.
    // Note the two documents also have different lengths, which the BM25
    // length normalisation (`b = 0.75`) rewards the SHORT one for — the point
    // of this assertion is only that the returned order is by score and not
    // by name, so the fixture uses the enough-repeats/other-longer shape
    // twice and asserts the winner each time.
    let ranker = Bm25Ranker::new(vec![
        (
            "zeta".to_owned(),
            "widget widget widget widget widget".to_owned(),
        ),
        ("alpha".to_owned(), "widget".to_owned()),
    ]);
    let ranked = ranker.rank("widget", 8);
    assert_eq!(ranked.len(), 2, "both documents match: {ranked:?}");
    assert!(
        (ranked[0].1 - ranked[1].1).abs() > 1e-12,
        "the fixture must produce distinct scores: {ranked:?}"
    );
    // The name order ("alpha" first) must NOT decide: whichever document
    // scores higher is first, and the fixture's frequent-term document is the
    // one that scores higher.
    assert_eq!(
        ranked[0].0, "zeta",
        "the score is the primary key: {ranked:?}"
    );

    // Mirror image: the frequent-term document named so it sorts FIRST.
    let ranker = Bm25Ranker::new(vec![
        (
            "aaa".to_owned(),
            "widget widget widget widget widget".to_owned(),
        ),
        ("zzz".to_owned(), "widget".to_owned()),
    ]);
    let ranked = ranker.rank("widget", 8);
    assert_eq!(
        ranked[0].0, "aaa",
        "the score is the primary key in both directions: {ranked:?}"
    );
}

/// The `limit` truncation happens AFTER the total ordering, so the returned
/// prefix is the same for every document order (a limit applied before the
/// sort would let insertion order pick the window).
#[cfg(feature = "tool-search")]
#[test]
fn bm25_limit_applies_after_the_total_order() {
    use pir::plugins::builtin::Bm25Ranker;

    let build = |order: [&str; 4]| -> Bm25Ranker {
        Bm25Ranker::new(
            order
                .into_iter()
                .map(|name| (name.to_owned(), "shared searchable text".to_owned()))
                .collect(),
        )
    };
    let forward = build(["delta", "charlie", "bravo", "alpha"]);
    let backwards = build(["alpha", "bravo", "charlie", "delta"]);
    assert_eq!(
        forward.rank("shared", 2),
        backwards.rank("shared", 2),
        "the truncated window must not depend on document order"
    );
    let names: Vec<String> = forward
        .rank("shared", 2)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        vec!["alpha".to_owned(), "bravo".to_owned()],
        "the first two names in the total order"
    );
}

// ---------------------------------------------------------------------------
// (4) Subagent description comes from the resolved agent dir
// ---------------------------------------------------------------------------

/// The default subagent tool renders its description from
/// `agent_dir()/agents` (the single source that honors `PIR_AGENT_DIR`), and
/// an explicit resolved directory is rendered verbatim — never the hardcoded
/// `~/.pi/agent/agents`.
#[cfg(feature = "subagent")]
#[test]
fn subagent_description_uses_the_resolved_agent_dir() {
    use pir::plugins::SubagentTool;

    let resolved = pir::coding_agent::context::agent_dir().join("agents");
    let default_description = SubagentTool::default().description().to_owned();
    assert!(
        default_description.contains(&resolved.display().to_string()),
        "the description must name the resolved agent dir {:?}: {default_description}",
        resolved
    );

    let explicit = std::env::temp_dir().join(format!(
        "pir-w11-subagent-agents-{}-resolved",
        std::process::id()
    ));
    let explicit_description = SubagentTool::with_extensions_and_dir(None, &explicit)
        .description()
        .to_owned();
    assert!(
        explicit_description.contains(&explicit.display().to_string()),
        "the description must come from the passed resolved dir: {explicit_description}"
    );
    assert!(
        !explicit_description.contains("~/.pi/agent/agents"),
        "no hardcoded agent dir: {explicit_description}"
    );
}

// ---------------------------------------------------------------------------
// (2) Native plugin fan-out: order, ToolContext, images, merged results
// ---------------------------------------------------------------------------

/// The caller's context a native plugin tool observes.
#[derive(Debug)]
struct SeenContext {
    aborted: bool,
    background: bool,
    cwd: PathBuf,
}

/// Native tool that records its [`ToolContext`] and returns text, an image,
/// details and structured content.
struct FanoutProbeTool {
    name: &'static str,
    text: &'static str,
    seen: Arc<Mutex<Option<SeenContext>>>,
}

impl AgentTool for FanoutProbeTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "W11-F fan-out probe"
    }

    fn run(&self, _args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SeenContext {
            aborted: ctx.is_aborted(),
            background: ctx.background,
            cwd: ctx.cwd.clone(),
        });
        let mut result = ToolResult::ok(self.text);
        result.images.push(ImageContent {
            data: "aW1n".to_owned(),
            mime_type: "image/png".to_owned(),
        });
        result.details = Some(json!({ "detail": self.text }));
        result.structured_content = Some(json!({ "structured": self.text }));
        Box::pin(async move { result })
    }
}

/// Inline plugin registering one tool and recording the events it observes.
struct FanoutPlugin {
    info: PluginInfo,
    tool: Option<Arc<dyn AgentTool>>,
    events_seen: Arc<Mutex<Vec<String>>>,
    label: &'static str,
}

impl AgentPlugin for FanoutPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        if let Some(tool) = &self.tool {
            ctx.register_tool(Arc::clone(tool));
        }
        Ok(())
    }

    /// This probe exists to observe the ORDERED fan-out for any event the test
    /// sends (it records the name and returns a generic handler result), so it
    /// declares `All`. The default `EventInterest::None` now admits nothing
    /// (`native_plugin_wants_event`, `plugins/host.rs`), which would leave the
    /// probe unobservable — the declaration, not an undeclared arm, is what
    /// makes a handler reachable.
    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::All
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let label = self.label;
        let seen = Arc::clone(&self.events_seen);
        let event_name = event.name.to_string();
        Box::pin(async move {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("{label}:{event_name}"));
            PluginEventOutcome::Generic(json!({ "handler": label }))
        })
    }
}

fn fanout_plugin(
    name: &'static str,
    label: &'static str,
    tool: Option<Arc<dyn AgentTool>>,
    events_seen: Arc<Mutex<Vec<String>>>,
) -> Arc<dyn AgentPlugin> {
    Arc::new(FanoutPlugin {
        info: PluginInfo::inline(name),
        tool,
        events_seen,
        label,
    })
}

/// The native tool call keeps the caller's `ToolContext` (abort signal,
/// background flag, cwd) and its image/details/structured content.
#[tokio::test(flavor = "current_thread")]
async fn native_fanout_preserves_caller_context_and_images() {
    let seen = Arc::new(Mutex::new(None));
    let events_seen = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![fanout_plugin(
            "w11-fanout-context",
            "first",
            Some(Arc::new(FanoutProbeTool {
                name: "fanout-probe",
                text: "probe-ok",
                seen: Arc::clone(&seen),
            })),
            events_seen,
        )],
    );

    let cwd = std::env::temp_dir().join(format!("pir-w11-fanout-cwd-{}", std::process::id()));
    let mut ctx = ToolContext::new(&cwd);
    let signal = AbortSignal::new();
    signal.abort();
    ctx.signal = Some(signal);
    ctx.background = true;

    let outcome = host
        .call_tool_with_context("fanout-probe", "call-1", &json!({}), &ctx)
        .await
        .expect("the native tool runs through the host");
    assert!(outcome
        .result
        .content
        .iter()
        .any(|item| { item.kind == "text" && item.text == "probe-ok" }));
    assert_eq!(
        outcome.images.len(),
        1,
        "the tool's image must reach the structured return"
    );
    assert_eq!(outcome.images[0].data, "aW1n");
    assert_eq!(
        outcome.result.details,
        json!({ "detail": "probe-ok" }),
        "details survive"
    );
    assert_eq!(
        outcome.result.structured_content,
        json!({ "structured": "probe-ok" }),
        "structured content survives"
    );
    assert!(
        outcome
            .result
            .content
            .iter()
            .any(|item| { item.kind == "image" && item.data.as_deref() == Some("aW1n") }),
        "the wire result carries the image block too: {:?}",
        outcome.result.content
    );

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the probe recorded its context");
    assert!(seen.aborted, "the caller's abort signal reaches the tool");
    assert!(
        seen.background,
        "the caller's background flag reaches the tool"
    );
    assert_eq!(seen.cwd, cwd, "the caller's cwd reaches the tool");
}

/// Event dispatch and tool resolution both follow the host's plugin list
/// order; the merged outcome is last-wins (later handlers observe earlier
/// payload mutations).
#[tokio::test(flavor = "current_thread")]
async fn native_fanout_preserves_host_order() {
    let events_seen = Arc::new(Mutex::new(Vec::new()));
    let shared_seen = Arc::new(Mutex::new(None));
    // Two plugins register the SAME tool name: first registration wins.
    let host = PluginHost::build(
        None,
        vec![
            fanout_plugin(
                "w11-order-first",
                "first",
                Some(Arc::new(FanoutProbeTool {
                    name: "ordered-probe",
                    text: "first-wins",
                    seen: Arc::clone(&shared_seen),
                })),
                Arc::clone(&events_seen),
            ),
            fanout_plugin(
                "w11-order-second",
                "second",
                Some(Arc::new(FanoutProbeTool {
                    name: "ordered-probe",
                    text: "second-loses",
                    seen: Arc::new(Mutex::new(None)),
                })),
                Arc::clone(&events_seen),
            ),
        ],
    );

    let mut payload = json!({ "type": "project_trust", "cwd": "." });
    let outcome = host
        .emit("project_trust", &mut payload)
        .await
        .expect("ordered emit");
    assert_eq!(
        *events_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            "first:project_trust".to_owned(),
            "second:project_trust".to_owned()
        ],
        "handlers run in host list order"
    );
    assert_eq!(
        outcome.result,
        json!({ "handler": "second" }),
        "generic results merge last-wins in dispatch order"
    );

    let ctx = ToolContext::new(std::env::temp_dir());
    let tool_outcome = host
        .call_tool_with_context("ordered-probe", "call-order", &json!({}), &ctx)
        .await
        .expect("the first registration wins the name");
    assert!(
        tool_outcome
            .result
            .content
            .iter()
            .any(|item| item.kind == "text" && item.text == "first-wins"),
        "the host list's first registration must resolve: {:?}",
        tool_outcome.result.content
    );
}

/// The ordered `emit` fan-out, not just the bare merge, keeps a native
/// `tool_result` handler's image blocks and does not lose the payload fields
/// earlier handlers set (the host-level shape of item 3's "does not drop
/// merged event-result images/messages").
#[tokio::test(flavor = "current_thread")]
async fn native_tool_result_fanout_keeps_image_blocks_and_order() {
    /// Handler that rewrites `tool_result` content with an image block and
    /// records the order it ran in.
    struct RewritePlugin {
        info: PluginInfo,
        label: &'static str,
        order: Arc<Mutex<Vec<String>>>,
        with_image: bool,
    }

    impl AgentPlugin for RewritePlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> pir::plugins::EventInterest {
            pir::plugins::EventInterest::List(vec!["tool_result".to_owned()])
        }

        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let label = self.label;
            let order = Arc::clone(&self.order);
            let with_image = self.with_image;
            let tool_name = event.tool_name().map(str::to_owned);
            Box::pin(async move {
                order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!("{label}:{tool_name:?}"));
                let mut content = vec![json!({ "type": "text", "text": label })];
                if with_image {
                    content.push(json!({
                        "type": "image",
                        "data": "aW1n",
                        "mimeType": "image/png",
                    }));
                }
                PluginEventOutcome::ToolResult {
                    content: Some(Value::Array(content)),
                    details: None,
                    structured_content: None,
                    is_error: None,
                    usage: None,
                }
            })
        }
    }

    let order = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![
            Arc::new(RewritePlugin {
                info: PluginInfo::inline("w11-tool-result-first"),
                label: "first",
                order: Arc::clone(&order),
                with_image: true,
            }),
            Arc::new(RewritePlugin {
                info: PluginInfo::inline("w11-tool-result-second"),
                label: "second",
                order: Arc::clone(&order),
                with_image: false,
            }),
        ],
    );
    assert!(
        host.has_native_handlers("tool_result"),
        "both handlers must be reachable without a JS runtime"
    );

    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "call-1",
        "toolName": "ordered-probe",
        "content": [{ "type": "text", "text": "original" }],
        "isError": false,
    });
    let outcome = host
        .emit("tool_result", &mut payload)
        .await
        .expect("ordered tool_result emit");
    assert_eq!(
        *order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            "first:Some(\"ordered-probe\")".to_owned(),
            "second:Some(\"ordered-probe\")".to_owned(),
        ],
        "tool_result handlers run in host list order"
    );
    // Last-wins content: the second handler replaced it, and the FIRST
    // handler's image is gone with the replaced content — a replacement, not
    // an accumulation, exactly like upstream's `tool_result` overlay.
    assert_eq!(
        outcome.result["content"],
        json!([{ "type": "text", "text": "second" }]),
        "the last content replacement wins: {:?}",
        outcome.result
    );

    // Single-handler case: the image block the handler wrote survives the
    // dispatch untouched (the merge never rewrites blocks).
    let order = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![Arc::new(RewritePlugin {
            info: PluginInfo::inline("w11-tool-result-image"),
            label: "only",
            order,
            with_image: true,
        })],
    );
    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "call-2",
        "toolName": "ordered-probe",
        "content": [{ "type": "text", "text": "original" }],
        "isError": false,
    });
    let outcome = host
        .emit("tool_result", &mut payload)
        .await
        .expect("single-handler tool_result emit");
    assert_eq!(
        outcome.result["content"],
        json!([
            { "type": "text", "text": "only" },
            { "type": "image", "data": "aW1n", "mimeType": "image/png" },
        ]),
        "the handler's image block reaches the merged result: {:?}",
        outcome.result
    );
    assert!(
        outcome.result.get("structuredContent").is_some(),
        "the structuredContent drop rule is applied by the merge: {:?}",
        outcome.result
    );
    assert_eq!(
        outcome.result["structuredContent"],
        Value::Null,
        "replacing content without structured content drops the stale value"
    );
}

/// The shared merge keeps `input` images across a later text-only transform,
/// accumulates `before_agent_start` messages in handler order, and preserves
/// image blocks in a `tool_result` content replacement.
#[test]
fn merged_event_results_keep_images_and_messages() {
    // `input`: a later transform without images must not drop the earlier
    // handler's images.
    let images = json!([{ "data": "aW1n", "mimeType": "image/png" }]);
    let mut acc = MergedEmit::new();
    acc.merge(
        "input",
        "first",
        PluginEventOutcome::Input {
            action: InputAction::Transform,
            text: Some("first text".to_owned()),
            images: Some(images.clone()),
        },
    );
    acc.merge(
        "input",
        "second",
        PluginEventOutcome::Input {
            action: InputAction::Transform,
            text: Some("second text".to_owned()),
            images: None,
        },
    );
    assert_eq!(acc.result["text"], "second text");
    assert_eq!(
        acc.result["images"], images,
        "the earlier transform's images survive the later text-only transform"
    );

    // `before_agent_start`: every handler's message accumulates in order.
    let first_message = json!({ "customType": "one", "content": "first" });
    let second_message = json!({ "customType": "two", "content": "second" });
    let mut acc = MergedEmit::new();
    acc.merge(
        "before_agent_start",
        "first",
        PluginEventOutcome::BeforeAgentStart {
            system_prompt: None,
            message: Some(first_message.clone()),
            messages: vec![first_message.clone()],
        },
    );
    acc.merge(
        "before_agent_start",
        "second",
        PluginEventOutcome::BeforeAgentStart {
            system_prompt: Some("prompt".to_owned()),
            message: Some(second_message.clone()),
            messages: Vec::new(),
        },
    );
    assert_eq!(
        acc.result["messages"],
        json!([first_message, second_message]),
        "messages accumulate across handlers"
    );
    assert_eq!(acc.result["systemPrompt"], "prompt");
    assert_eq!(acc.result["message"], second_message, "last message wins");

    // `tool_result`: an image block in the replacement content survives the
    // merge (the host consumer flattens text; the merge itself must not drop
    // the blocks).
    let replacement = json!([
        { "type": "text", "text": "caption" },
        { "type": "image", "data": "aW1n", "mimeType": "image/png" },
    ]);
    let mut acc = MergedEmit::new();
    acc.merge(
        "tool_result",
        "first",
        PluginEventOutcome::ToolResult {
            content: Some(replacement.clone()),
            details: None,
            structured_content: None,
            is_error: None,
            usage: None,
        },
    );
    assert_eq!(
        acc.result["content"], replacement,
        "the merged content keeps its image block"
    );
    assert_eq!(
        acc.result["structuredContent"],
        Value::Null,
        "replacing content without structured content drops the stale value"
    );
}
