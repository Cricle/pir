//! W12-F agent-core residual pins (unregistered suite; the foreman registers
//! it in a `g_*` binary).
//!
//! Each test below pins one PLAN §6b.3 / §5f-R5 residual against the CURRENT
//! code and fails if the corresponding guarantee is reverted:
//!
//! 1. `a_completion_never_persists_inside_an_open_tool_batch` — the agent loop
//!    holds [`CommitTracker`]'s batch barrier from the assistant message that
//!    carries a tool batch through that batch's LAST result write, so a
//!    background completion whose placeholder gate already opened still waits
//!    for the batch boundary (B1).
//! 2. `commit_trackers_are_per_runtime_so_sessions_cannot_open_each_others_gates`
//!    — two runs using the SAME provider call id with their own trackers; one
//!    run's commit must never open the other runtime's placeholder gate.
//! 3. `a_settling_run_persists_once_and_never_queues` — the settle transition
//!    (close the enqueue gate, then drain) plus the bounded delivered-id ring:
//!    one completion id can persist + queue at most once, and a completion
//!    that lost the close transition is neither queued nor in the drain;
//!    `a_settle_retry_persists_exactly_the_unhandled_messages` pins the
//!    per-ordinal persist ledger (no duplicate, no loss on a retry).
//! 4. `native_message_update_payload_is_o_delta` and
//!    `finish_turn_sees_the_runs_shared_transcript` — the native per-delta
//!    fan-out never materializes the accumulated reply, and the per-turn hook
//!    receives `Arc` handles (a refcount bump), not deep clones.
//! 5. `dropping_the_execute_bash_future_kills_the_whole_process_tree` — the
//!    out-of-loop bash path arms an RAII tree-kill guard at spawn, so a
//!    cancelled future (RPC disconnect, host task abort) still kills the
//!    command's forked descendants, not just the shell.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::agent_loop::{
    message_update_payload, native_message_update_payload, run_with_options, FinishTurnAction,
    FinishTurnContext, FinishTurnHook, LoopDeps, LoopOptions, ProviderRequest,
};
use pir::agent::commit::CommitTracker;
use pir::agent::tool_batch::{ToolExecutionMode, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::agent_session::{FollowUpQueueProbe, PersistLedger};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, StopReason,
    TextContent, ToolCall, UserMessage, UserRole,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.to_owned(),
        text_signature: None,
    })
}

fn tool_call_block(id: &str, name: &str) -> ContentBlock {
    let mut arguments = BTreeMap::new();
    arguments.insert("value".to_owned(), json!(1));
    ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// A probe tool: records one run and answers `ok`.
struct EchoTool {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo-probe"
    }

    fn description(&self) -> &'static str {
        "echo probe"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { ToolResult::ok("echo-probe-ok") })
    }
}

fn echo_tools(runs: &Arc<AtomicUsize>) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool {
        runs: Arc::clone(runs),
    }));
    tools
}

/// The call id of a tool-result message (the loop's persistence order key).
fn result_call_id(message: &Message) -> Option<String> {
    match message {
        Message::ToolResult(result) => Some(result.tool_call_id.clone()),
        _ => None,
    }
}

/// One scripted stream: the first request answers with `calls` (ToolUse), every
/// later request answers `done`. No provider is ever touched.
fn scripted_stream(calls: Vec<ContentBlock>) -> pir::agent::agent_loop::StreamFn {
    let calls_made = Arc::new(AtomicUsize::new(0));
    Box::new(move |_request: ProviderRequest| {
        let index = calls_made.fetch_add(1, Ordering::SeqCst);
        let message = if index == 0 {
            assistant(calls.clone(), StopReason::ToolUse)
        } else {
            assistant(vec![text("done")], StopReason::Stop)
        };
        Box::pin(async move { message })
    })
}

fn initial_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("go")],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 1,
        })],
        tools: None,
    }
}

use pir::ai::types::{Model, ModelCost, ThinkingLevel};

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: pir::ai::types::ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4_096,
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

/// Per-test scratch directory (session storage / cwd).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-agent-core-w11-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A session whose provider always finalizes `done` (no tool calls), used by
/// the settlement-window race test.
fn settle_abort_config(temp: &TempDir) -> pir::agent_session::AgentSessionConfig {
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::types::TerminalReason;

    let stream: pir::agent_session::SharedEventStreamFn = Arc::new(move |_request| {
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text("done")], StopReason::Stop),
            ));
            stream
        })
    });
    pir::agent_session::AgentSessionConfig {
        provider_registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: Some(stream),
        extensions: None,
        tool_selection: None,
    }
}

// ---------------------------------------------------------------------------
// 1. Batch barrier (B1): a completion never persists inside an open batch
// ---------------------------------------------------------------------------

/// A batch of two calls, run sequentially. The completion waiter is registered
/// for `call-1` BEFORE the run, so the loop opens its placeholder gate once
/// call-1's result is persisted — while call-2's result is still being written.
/// The waiter must still be blocked at that point: the loop holds the batch
/// barrier until the batch's last result write, so the completion's custom
/// entry can only land at a batch boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completion_never_persists_inside_an_open_tool_batch() {
    let runs = Arc::new(AtomicUsize::new(0));
    let tracker = CommitTracker::new();
    let gate = Arc::new(tracker.register_for_test(vec!["call-1".to_owned()]));

    // What `BackgroundDelivery::finish` does after its A1 gate: acquire the
    // batch boundary, then write. The snapshot it takes of the persisted
    // prefix is where its custom entry would land in the transcript.
    let persisted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_prefix: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let waiter = {
        let tracker = Arc::clone(&tracker);
        let persisted = Arc::clone(&persisted);
        let observed_prefix = Arc::clone(&observed_prefix);
        tokio::spawn(async move {
            gate.wait().await;
            let _boundary = tracker.wait_for_batch_boundary_for_test().await;
            let prefix = persisted.lock().unwrap().clone();
            *observed_prefix.lock().unwrap() = Some(prefix);
        })
    };

    let result = run_with_options(
        LoopDeps {
            stream: scripted_stream(vec![
                tool_call_block("call-1", "echo-probe"),
                tool_call_block("call-2", "echo-probe"),
            ]),
            tools: Arc::new(echo_tools(&runs)),
            scheduler: ToolScheduler::new(ToolSchedulerConfig {
                mode: ToolExecutionMode::Sequential,
                ..Default::default()
            }),
            commit_tracker: Some(Arc::clone(&tracker)),
            max_turns: 4,
            ..LoopDeps::default()
        },
        initial_context(),
        None,
        LoopOptions {
            persist_message: Some({
                let persisted = Arc::clone(&persisted);
                Arc::new(move |message: Message| {
                    let persisted = Arc::clone(&persisted);
                    Box::pin(async move {
                        let Some(call_id) = result_call_id(&message) else {
                            return;
                        };
                        if call_id == "call-2" {
                            // Widen the window: a completion that wrongly ran
                            // on the A1 gate alone gets the CPU here, while the
                            // batch's last result is still unpersisted.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        persisted.lock().unwrap().push(call_id);
                    })
                })
            }),
            ..LoopOptions::default()
        },
    )
    .await;

    assert_eq!(runs.load(Ordering::SeqCst), 2, "both calls ran");
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);

    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the completion must be released once the batch closes")
        .expect("completion task panicked");

    let order = persisted.lock().unwrap().clone();
    assert_eq!(
        order,
        vec!["call-1".to_owned(), "call-2".to_owned()],
        "the loop persists the batch's results in call order"
    );
    let prefix = observed_prefix
        .lock()
        .unwrap()
        .clone()
        .expect("the completion task ran");
    assert_eq!(
        prefix, order,
        "the completion was released before the batch's last result was persisted: \
         its custom entry would split the batch on resume (B1)"
    );
}

// ---------------------------------------------------------------------------
// 2. Per-runtime commit gates: two sessions with the same provider call id
// ---------------------------------------------------------------------------

/// Two runs, the SAME provider call id, one `CommitTracker` each. Only the
/// owning runtime's loop may open its own gate; the other runtime's gate stays
/// closed until its own run commits the id. A process-global gate table keyed
/// by call id (the pre-fix shape) opens both at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_trackers_are_per_runtime_so_sessions_cannot_open_each_others_gates() {
    let tracker_a = CommitTracker::new();
    let tracker_b = CommitTracker::new();
    let gate_a = Arc::new(tracker_a.register_for_test(vec!["call-shared".to_owned()]));
    let gate_b = Arc::new(tracker_b.register_for_test(vec!["call-shared".to_owned()]));

    let run_one = |tracker: Arc<CommitTracker>| {
        let runs = Arc::new(AtomicUsize::new(0));
        run_with_options(
            LoopDeps {
                stream: scripted_stream(vec![tool_call_block("call-shared", "echo-probe")]),
                tools: Arc::new(echo_tools(&runs)),
                commit_tracker: Some(tracker),
                max_turns: 4,
                ..LoopDeps::default()
            },
            initial_context(),
            None,
            LoopOptions::default(),
        )
    };

    run_one(Arc::clone(&tracker_a)).await;
    assert!(
        gate_a.is_committed(),
        "session A's own run commits its gate"
    );
    assert!(
        !gate_b.is_committed(),
        "session A's run opened session B's gate for the same provider call id"
    );

    run_one(Arc::clone(&tracker_b)).await;
    assert!(
        gate_b.is_committed(),
        "session B's own run commits its gate"
    );
}

// ---------------------------------------------------------------------------
// 3. Settle path: one delivery, one frame, exact ledger
// ---------------------------------------------------------------------------

/// Idempotent delivery: the completion-id marker admits one persist+queue, a
/// frame accepted before the settle transition is drained exactly once, and a
/// frame refused after it is neither queued nor drained.
#[tokio::test]
async fn a_settling_run_persists_once_and_never_queues() {
    let probe = FollowUpQueueProbe::new();

    // Live run: the frame queues once; a duplicate delivery of the same id is
    // refused before it could persist or queue again.
    assert!(probe.mark_delivered("completion-1"));
    assert!(probe.try_queue("frame-1"));
    assert!(
        !probe.mark_delivered("completion-1"),
        "a second delivery of the same completion must be refused \
         (no second persist, no second frame)"
    );
    assert_eq!(probe.queued().await, vec!["frame-1".to_owned()]);

    // Settle: close FIRST, then drain (the `mark_settling` order).
    probe.close();
    assert!(
        !probe.try_queue("frame-late"),
        "a completion landing after the settle transition must not queue"
    );
    assert_eq!(
        probe.drain_all().await,
        vec!["frame-1".to_owned()],
        "the drain must carry exactly the frames accepted while the run was live"
    );
    assert!(probe.queued().await.is_empty());

    // The next prompt reopens: a new completion delivers normally.
    probe.open();
    assert!(probe.mark_delivered("completion-2"));
    assert!(probe.try_queue("frame-2"));
    assert_eq!(probe.drain_all().await, vec!["frame-2".to_owned()]);
}

/// The settle retry persists exactly the messages the sink did not handle: a
/// run whose middle append failed re-appends the tail in produced order (no
/// duplicate, no loss), and an intentional omission past the failure barrier
/// is still skipped by its own ordinal rather than by "index < count".
#[test]
fn a_settle_retry_persists_exactly_the_unhandled_messages() {
    let ledger = PersistLedger::new_for_test();
    let ordinals: Vec<usize> = (0..5).map(|_| ledger.claim_for_test()).collect();
    assert_eq!(ordinals, vec![0, 1, 2, 3, 4]);

    // 0 committed, 1's append failed, 2 omitted (overflow recovery), 3's sink
    // call was blocked by the failure barrier, 4 committed after a recovery.
    ledger.handle_for_test(0);
    ledger.fail_for_test(1);
    ledger.handle_for_test(2);
    assert!(ledger.blocked_for_test(3), "later sink writes are blocked");
    assert!(!ledger.is_handled_for_test(3));
    ledger.handle_for_test(4);

    let plan: Vec<usize> = (0..5)
        .filter(|ordinal| !ledger.is_handled_for_test(*ordinal))
        .collect();
    assert_eq!(
        plan,
        vec![1, 3],
        "the batch must write exactly the messages the sink never persisted, in order"
    );

    // Resume: the next settle iteration starts clean; no ordinal from the
    // previous iteration leaks into it.
    ledger.reset_for_test();
    assert!((0..5).all(|ordinal| !ledger.is_handled_for_test(ordinal)));
}

// ---------------------------------------------------------------------------
// 3b. The settle transition (`run_settling`) has no lost-abort race
// ---------------------------------------------------------------------------

/// An `agent_settled` handler that parks the run inside its post-run
/// settlement window until the test releases it.
struct SettleGatePlugin {
    info: pir::plugins::PluginInfo,
    entered: Arc<AtomicUsize>,
    gate: tokio::sync::watch::Receiver<bool>,
}

impl pir::plugins::AgentPlugin for SettleGatePlugin {
    fn info(&self) -> &pir::plugins::PluginInfo {
        &self.info
    }

    fn events(&self) -> pir::plugins::EventInterest {
        pir::plugins::EventInterest::List(vec!["agent_settled".into()])
    }

    fn on_event<'a>(
        &'a self,
        _event: pir::plugins::PluginEvent<'a>,
    ) -> pir::plugins::PluginFuture<'a, pir::plugins::PluginEventOutcome> {
        let entered = Arc::clone(&self.entered);
        let mut gate = self.gate.clone();
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if *gate.borrow() {
                        break;
                    }
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            pir::plugins::PluginEventOutcome::None
        })
    }
}

/// An `abort()` landing while the run sits in its post-run settlement window
/// must not leak into the NEXT run: the guard clears the run-abort request
/// before it releases the claim (claim LAST), and `start_run` clears any
/// leftover request when it takes a fresh claim. A lost-abort ordering (claim
/// released before the flags reset) either kills the next run or leaves it
/// permanently "aborting".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_in_the_settling_window_never_reaches_the_next_run() {
    let temp = TempDir::new("settle-abort");
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let agent = Arc::new(pir::agent_session::AgentSession::new_with_plugins(
        settle_abort_config(&temp),
        vec![Arc::new(SettleGatePlugin {
            info: pir::plugins::PluginInfo::inline("settle-gate"),
            entered: Arc::clone(&entered),
            gate: gate_rx,
        })],
    ));

    let prompt_task = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent
                .prompt(
                    "first",
                    Arc::new(|_event| {}) as pir::agent_session::EventHandler,
                )
                .await
        }
    });
    // Parked in the settlement window: the agent loop already produced its
    // terminal result, the run's future is still alive.
    let started = std::time::Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the run never reached its settlement window"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        agent.is_streaming(),
        "the claim is still held while settling"
    );

    // The abort under test: the run is over, but the claim is RUNNING, so the
    // abort records a run-abort request.
    agent.abort();
    let _ = gate_tx.send(true);

    let first = prompt_task
        .await
        .expect("prompt task")
        .expect("the first run must still settle normally");
    assert_eq!(first.stop_reason, StopReason::Stop);
    assert!(!agent.is_streaming(), "the claim was released");

    // The next run must start clean: neither the stale abort request nor the
    // settled run's aborted signal may stop it.
    let second = agent
        .prompt(
            "second",
            Arc::new(|_event| {}) as pir::agent_session::EventHandler,
        )
        .await
        .expect("the second run must not be aborted by the stale request");
    assert_eq!(second.stop_reason, StopReason::Stop);
    assert_eq!(
        second.content.first().and_then(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        }),
        Some("done"),
        "the second run must reach the provider and finish"
    );
}

// ---------------------------------------------------------------------------
// 4. Native per-delta payload is O(delta); per-turn hooks share the transcript
// ---------------------------------------------------------------------------

/// The native (Rust plugin) `message_update` fan-out must never materialize
/// the accumulated reply: a delta on a 512 KiB reply costs the same wire as a
/// delta on a 1 KiB reply. The reference construction grows with the reply —
/// this test also proves the two routes have not been collapsed back into one.
#[test]
fn native_message_update_payload_is_o_delta() {
    fn delta_event(accumulated: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "chunk".into(),
            partial: Arc::new(assistant(vec![text(accumulated)], StopReason::Pending)),
        }
    }

    let small = delta_event(&"x".repeat(1_024));
    let large = delta_event(&"x".repeat(512 * 1_024));

    let small_native = serde_json::to_string(&native_message_update_payload(&small)).unwrap();
    let large_native = serde_json::to_string(&native_message_update_payload(&large)).unwrap();
    assert_eq!(
        small_native.len(),
        large_native.len(),
        "the native per-delta payload grew with the accumulated reply \
         (O(reply) per delta = O(reply^2) per reply)"
    );
    assert!(
        !large_native.contains(&"x".repeat(64)),
        "the native per-delta payload carries the accumulated content"
    );
    // The reference construction is still the full snapshot (terminal events
    // and the JS equivalence tests use it); the delta route must not be it.
    let reference = serde_json::to_string(&message_update_payload(&large)).unwrap();
    assert!(
        reference.len() >= 512 * 1_024,
        "the reference construction must stay the full-snapshot value"
    );
}

/// `finish_turn` gets the run's LIVE transcript and produced-message list as
/// `Arc` handles. A deep clone (the pre-fix `context.clone()` / `to_vec()`
/// shape) would hand the hook a fresh allocation with a strong count of 1.
#[tokio::test(flavor = "current_thread")]
async fn finish_turn_sees_the_runs_shared_transcript() {
    let seen: Arc<Mutex<Vec<(usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: FinishTurnHook = {
        let seen = Arc::clone(&seen);
        Arc::new(move |context: FinishTurnContext| {
            let counts = (
                Arc::strong_count(&context.context),
                Arc::strong_count(&context.new_messages),
            );
            seen.lock().unwrap().push(counts);
            Box::pin(async { None::<FinishTurnAction> })
        })
    };

    let runs = Arc::new(AtomicUsize::new(0));
    let result = run_with_options(
        LoopDeps {
            stream: scripted_stream(vec![tool_call_block("call-1", "echo-probe")]),
            tools: Arc::new(echo_tools(&runs)),
            finish_turn: Some(hook),
            max_turns: 4,
            ..LoopDeps::default()
        },
        initial_context(),
        None,
        LoopOptions::default(),
    )
    .await;
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);

    let observed = seen.lock().unwrap().clone();
    assert_eq!(observed.len(), 2, "one hook call per turn");
    for (turn, (context_count, messages_count)) in observed.iter().enumerate() {
        assert!(
            *context_count >= 2,
            "turn {}: finish_turn received a DEEP-COPIED context (strong count {context_count})",
            turn + 1
        );
        assert!(
            *messages_count >= 2,
            "turn {}: finish_turn received a DEEP-COPIED produced list (strong count {messages_count})",
            turn + 1
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Out-of-loop bash: Drop kills the whole tree
// ---------------------------------------------------------------------------

use std::path::PathBuf;

/// Hermetic `PIR_AGENT_DIR` pin shared with every other suite in this merged
/// binary: the env is process-global, so a module-local lock would not exclude
/// the SDK suites' own pins (`tests/common/mod.rs::AgentDirEnvGuard`).
async fn hermetic_agent_dir(label: &str) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::scratch(label, None).await
}

/// A scratch cwd for the probe's `pid.txt`.
struct ProbeFixture {
    dir: PathBuf,
}

impl ProbeFixture {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-agent-core-w11-probe-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create probe dir");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for ProbeFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Publish the command's own OS process ids, then block in `wait` on a forked
/// `sleep`. Lines are `<msys pid>:<OS pid>`.
const BASH_PID_PROBE_COMMAND: &str = "sleep 30 &\nsub=${BASHPID:-$$}\n{\necho \"$$:$(cat /proc/$$/winpid 2>/dev/null)\"\necho \"$sub:$(cat /proc/$sub/winpid 2>/dev/null)\"\necho \"$!:$(cat /proc/$!/winpid 2>/dev/null)\"\n} > pid.tmp\nmv -f pid.tmp pid.txt\nwait\n";

/// Poll (bounded) until the probe published at least two `<pid>:<winpid>`
/// pairs, returning them.
async fn wait_for_probe_pids(path: &std::path::Path, deadline: Duration) -> Vec<String> {
    let started = std::time::Instant::now();
    loop {
        if let Ok(body) = std::fs::read_to_string(path) {
            let pairs: Vec<String> = body
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
            "the probe never published its pids to {} within {deadline:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Whether one OS pid is alive, through the platform's own process view
/// (`tasklist` on Windows, `kill -0` elsewhere).
fn os_pid_alive(pid: &str) -> bool {
    if pid.is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(pid),
            Err(_) => true,
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// The OS pid of one `<shell pid>:<OS pid>` pair published by the probe.
///
/// `/proc/<pid>/winpid` is an MSYS-only mapping; on Linux (and other POSIX
/// hosts) the file is absent and the probe publishes an empty second field.
/// There the shell pid IS the OS pid, so fall back to it instead of probing
/// the empty string (which would report every live process as gone).
fn probe_os_pid(pair: &str) -> &str {
    match pair.split_once(':') {
        Some((_shell_pid, os_pid)) if !os_pid.is_empty() => os_pid,
        Some((shell_pid, _)) => shell_pid,
        None => pair,
    }
}

/// Wait (bounded) until every published process is gone; returns the survivors
/// on success (empty).
async fn wait_for_all_gone(pids: &[String], deadline: Duration) -> Vec<String> {
    let started = std::time::Instant::now();
    loop {
        let alive: Vec<String> = pids
            .iter()
            .filter(|pair| os_pid_alive(probe_os_pid(pair)))
            .cloned()
            .collect();
        if alive.is_empty() {
            return alive;
        }
        assert!(
            started.elapsed() < deadline,
            "processes still alive after {deadline:?}: {alive:?} (published {pids:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn bash_available() -> bool {
    pir::coding_agent::tools::resolve_shell_config(None).is_ok()
}

fn bash_drop_session(cwd: PathBuf) -> pir::agent_session::AgentSession {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::provider::ProviderRegistry;

    AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(cwd),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    })
}

/// Cancelling the `execute_bash` future (a closed RPC bridge, a host aborting
/// its prompt task) must kill the command's WHOLE tree: the shell's
/// `kill_on_drop` reaches only the root, so without the RAII guard the forked
/// `sleep` survives the dropped future.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_execute_bash_future_kills_the_whole_process_tree() {
    let _agent_dir = hermetic_agent_dir("bash-drop").await;
    if !bash_available() {
        return;
    }
    let fixture = ProbeFixture::new("bash-drop");
    let session = Arc::new(bash_drop_session(fixture.dir.clone()));

    let task = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.execute_bash(BASH_PID_PROBE_COMMAND).await }
    });

    let pids = wait_for_probe_pids(&fixture.path("pid.txt"), Duration::from_secs(30)).await;
    // Self-validation: every published process must be observable before the
    // cancellation, so "gone" afterwards means the kill reached them.
    let alive = pids
        .iter()
        .filter(|pair| os_pid_alive(probe_os_pid(pair)))
        .count();
    assert_eq!(
        alive,
        pids.len(),
        "the probe's processes must be alive before the cancellation ({pids:?})"
    );

    // The cancellation itself: dropping the future is exactly what a closed
    // RPC bridge or a host `prompt_task.abort()` does.
    task.abort();
    let _ = task.await;

    wait_for_all_gone(&pids, Duration::from_secs(20)).await;
}
