//! Streaming UX: a parallel tool batch reports each call's lifecycle at the
//! moment it happens, not when the slowest sibling finishes.
//!
//! The fast (50 ms) call's `tool_execution_end` must be observable >500 ms
//! before the slow (800 ms) one and well before the batch returns. Starts are
//! observed before any call executes; the transcript stays in source order;
//! each face (host `AgentEvent` and extension fan-out) sees exactly one start
//! and one end per call. The fail-fast and abort paths still close every call
//! with an end.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::agent_loop::{
    run_with_options, AgentEvent, EventHandler, LoopDeps, LoopOptions, StreamFn,
};
use pir::agent::tool_batch::{AgentToolCall, ToolExecutionMode, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::{ToolExecutionProgress, ToolScheduler, ToolSchedulerConfig};
use pir::ai::types::{
    AbortSignal, AssistantMessage, ContentBlock, Context, StopReason, TextContent, ToolCall,
};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost,
    PluginInfo,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A tool that records when each run started, then sleeps for its delay.
struct TimedTool {
    name: &'static str,
    delay: Duration,
    starts: Arc<Mutex<Vec<Instant>>>,
}

impl AgentTool for TimedTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "timed test tool"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let delay = self.delay;
        let name = self.name;
        let starts = Arc::clone(&self.starts);
        Box::pin(async move {
            starts.lock().unwrap().push(Instant::now());
            tokio::time::sleep(delay).await;
            ToolResult::ok(format!("{name} done"))
        })
    }
}

struct FailingTool;

impl AgentTool for FailingTool {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn description(&self) -> &'static str {
        "always fails"
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::error("boom") })
    }
}

fn call(index: usize, id: &str, name: &str) -> AgentToolCall {
    AgentToolCall::new(index, id, name, json!({}))
}

fn tool_call_block(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

/// A native plugin recording every extension event with its arrival time.
struct EventRecorder {
    info: PluginInfo,
    events: Arc<Mutex<Vec<(Instant, String, Value)>>>,
}

impl EventRecorder {
    fn new(events: Arc<Mutex<Vec<(Instant, String, Value)>>>) -> Self {
        Self {
            info: PluginInfo::inline("streaming-recorder"),
            events,
        }
    }
}

impl AgentPlugin for EventRecorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::All
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.events.lock().unwrap().push((
                Instant::now(),
                event.name.to_string(),
                event.payload.clone(),
            ));
            PluginEventOutcome::None
        })
    }
}

// ---------------------------------------------------------------------------
// Streaming ordering through the agent loop
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_ends_stream_at_completion_and_transcript_stays_source_ordered() {
    let fast_starts = Arc::new(Mutex::new(Vec::new()));
    let slow_starts = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TimedTool {
        name: "fast",
        delay: Duration::from_millis(50),
        starts: Arc::clone(&fast_starts),
    }));
    tools.register(Arc::new(TimedTool {
        name: "slow",
        delay: Duration::from_millis(800),
        starts: Arc::clone(&slow_starts),
    }));

    // Source order is slow, fast; completion order is fast, slow.
    let calls = Arc::new(AtomicUsize::new(0));
    let stream: StreamFn = Box::new(move |_request| {
        let turn = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if turn == 0 {
                assistant(
                    vec![
                        tool_call_block("call-slow", "slow"),
                        tool_call_block("call-fast", "fast"),
                    ],
                    StopReason::ToolUse,
                )
            } else {
                assistant(
                    vec![ContentBlock::Text(TextContent {
                        text: "done".into(),
                        text_signature: None,
                    })],
                    StopReason::Stop,
                )
            }
        })
    });

    let observed: Arc<Mutex<Vec<(Instant, AgentEvent)>>> = Arc::new(Mutex::new(Vec::new()));
    let extension_events: Arc<Mutex<Vec<(Instant, String, Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![Arc::new(EventRecorder::new(Arc::clone(&extension_events)))],
    );
    let handler: EventHandler = {
        let observed = Arc::clone(&observed);
        Arc::new(move |event| {
            observed.lock().unwrap().push((Instant::now(), event));
        })
    };
    let deps = LoopDeps {
        stream,
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig {
            mode: ToolExecutionMode::Parallel,
            max_parallel: 8,
            ..ToolSchedulerConfig::default()
        }),
        plugin_events: Some(Arc::new(host)),
        max_turns: 4,
        ..LoopDeps::default()
    };

    let result = run_with_options(
        deps,
        Context::default(),
        None,
        LoopOptions {
            events: Some(handler),
            ..LoopOptions::default()
        },
    )
    .await;
    let returned = Instant::now();

    // --- per-call host events -------------------------------------------------
    let events = observed.lock().unwrap();
    let starts: Vec<(Instant, String)> = events
        .iter()
        .filter_map(|(at, event)| match event {
            AgentEvent::ToolExecutionStart { event, .. } => Some((*at, start_id(event))),
            _ => None,
        })
        .collect();
    let ends: Vec<(Instant, String)> = events
        .iter()
        .filter_map(|(at, event)| match event {
            AgentEvent::ToolExecutionEnd { event, .. } => Some((*at, end_id(event))),
            _ => None,
        })
        .collect();

    assert_eq!(starts.len(), 2, "one start per call: {starts:?}");
    assert_eq!(ends.len(), 2, "one end per call: {ends:?}");
    for id in ["call-slow", "call-fast"] {
        assert_eq!(
            starts.iter().filter(|(_, call_id)| call_id == id).count(),
            1,
            "duplicate start for {id}: {starts:?}"
        );
        assert_eq!(
            ends.iter().filter(|(_, call_id)| call_id == id).count(),
            1,
            "duplicate end for {id}: {ends:?}"
        );
    }
    // Starts are emitted in source order; ends in completion order.
    assert_eq!(
        starts.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(),
        vec!["call-slow", "call-fast"]
    );
    assert_eq!(
        ends.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(),
        vec!["call-fast", "call-slow"]
    );

    let fast_end = ends
        .iter()
        .find(|(_, id)| id == "call-fast")
        .map(|(at, _)| *at)
        .expect("fast end");
    let slow_end = ends
        .iter()
        .find(|(_, id)| id == "call-slow")
        .map(|(at, _)| *at)
        .expect("slow end");
    let last_start = starts.iter().map(|(at, _)| *at).max().expect("starts");
    let first_end = ends.iter().map(|(at, _)| *at).min().expect("ends");
    assert!(
        last_start < first_end,
        "every start must be observed before the first end"
    );
    assert!(
        slow_end.duration_since(fast_end) > Duration::from_millis(500),
        "fast end must stream >500 ms before the slow end (fast {fast_end:?}, slow {slow_end:?})"
    );
    assert!(
        returned.duration_since(fast_end) > Duration::from_millis(500),
        "fast end must be observed well before the batch/run returns"
    );

    // --- starts are observed before either tool begins executing -------------
    let tool_starts: Vec<Instant> = fast_starts
        .lock()
        .unwrap()
        .iter()
        .chain(slow_starts.lock().unwrap().iter())
        .copied()
        .collect();
    assert_eq!(tool_starts.len(), 2, "both tools ran exactly once");
    let earliest_tool_start = tool_starts.iter().min().copied().expect("tool start");
    assert!(
        last_start <= earliest_tool_start,
        "start events must be observed before execution begins (last start {last_start:?}, first run {earliest_tool_start:?})"
    );

    // --- extension face: exactly one start/end per call, same ordering -------
    let extension_events = extension_events.lock().unwrap();
    let ext_starts: Vec<&str> = extension_events
        .iter()
        .filter(|(_, name, _)| name == "tool_execution_start")
        .filter_map(|(_, _, payload)| payload["toolCallId"].as_str())
        .collect();
    let ext_ends: Vec<&str> = extension_events
        .iter()
        .filter(|(_, name, _)| name == "tool_execution_end")
        .filter_map(|(_, _, payload)| payload["toolCallId"].as_str())
        .collect();
    assert_eq!(ext_starts, vec!["call-slow", "call-fast"]);
    assert_eq!(ext_ends, vec!["call-fast", "call-slow"]);
    for id in ["call-slow", "call-fast"] {
        assert_eq!(
            ext_starts.iter().filter(|call_id| **call_id == id).count(),
            1
        );
        assert_eq!(ext_ends.iter().filter(|call_id| **call_id == id).count(), 1);
    }

    // --- transcript remains source-ordered ------------------------------------
    let ordered: Vec<(&str, &str)> = result
        .tool_results
        .iter()
        .map(|tool_result| {
            (
                tool_result.tool_call_id.as_str(),
                tool_result.tool_name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        ordered,
        vec![("call-slow", "slow"), ("call-fast", "fast")],
        "tool results must stay in assistant source order"
    );
}

fn start_id(event: &pir::agent::tool_batch::ToolExecutionEvent) -> String {
    match event {
        pir::agent::tool_batch::ToolExecutionEvent::Start { call_id, .. } => call_id.clone(),
        pir::agent::tool_batch::ToolExecutionEvent::End { call_id, .. } => call_id.clone(),
    }
}

fn end_id(event: &pir::agent::tool_batch::ToolExecutionEvent) -> String {
    start_id(event)
}

// ---------------------------------------------------------------------------
// Scheduler-level: sequential order, fail-fast, abort
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ProgressRecord {
    call_id: String,
    is_start: bool,
    is_error: bool,
    content: Option<String>,
}

fn progress_recorder() -> (ToolExecutionProgress, Arc<Mutex<Vec<ProgressRecord>>>) {
    let records = Arc::new(Mutex::new(Vec::new()));
    let sink: ToolExecutionProgress = {
        let records = Arc::clone(&records);
        Arc::new(move |event, finalized| {
            let (call_id, is_start) = match &event {
                pir::agent::tool_batch::ToolExecutionEvent::Start { call_id, .. } => {
                    (call_id.clone(), true)
                }
                pir::agent::tool_batch::ToolExecutionEvent::End { call_id, .. } => {
                    (call_id.clone(), false)
                }
            };
            records.lock().unwrap().push(ProgressRecord {
                call_id,
                is_start,
                is_error: finalized.is_some_and(|finalized| finalized.is_error()),
                content: finalized.map(|finalized| finalized.result.content.clone()),
            });
        })
    };
    (sink, records)
}

#[tokio::test(flavor = "multi_thread")]
async fn sequential_progress_reports_source_order() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TimedTool {
        name: "slow",
        delay: Duration::from_millis(120),
        starts: Arc::new(Mutex::new(Vec::new())),
    }));
    tools.register(Arc::new(TimedTool {
        name: "fast",
        delay: Duration::from_millis(10),
        starts: Arc::new(Mutex::new(Vec::new())),
    }));
    let (sink, records) = progress_recorder();
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Sequential,
        ..ToolSchedulerConfig::default()
    })
    .with_progress(sink);

    let outcome = scheduler
        .execute_batch(
            vec![call(0, "call-slow", "slow"), call(1, "call-fast", "fast")],
            &tools,
            &ToolContext::new("."),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 2);

    let records = records.lock().unwrap();
    let ids: Vec<&str> = records
        .iter()
        .map(|record| record.call_id.as_str())
        .collect();
    // Preflight reports every start first (source order); execution then
    // reports each end in source order.
    assert_eq!(
        ids,
        vec!["call-slow", "call-fast", "call-slow", "call-fast"],
        "starts then ends, both in source order"
    );
    assert!(records[0].is_start && records[1].is_start);
    assert!(!records[2].is_start && !records[3].is_start);
    assert_eq!(records[2].call_id, "call-slow");
    assert_eq!(records[3].call_id, "call-fast");
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_fast_emits_one_end_for_executed_and_skipped_calls() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FailingTool));
    tools.register(Arc::new(TimedTool {
        name: "slow",
        delay: Duration::from_millis(200),
        starts: Arc::new(Mutex::new(Vec::new())),
    }));
    let (sink, records) = progress_recorder();
    let scheduler = ToolScheduler::new(ToolSchedulerConfig {
        mode: ToolExecutionMode::Parallel,
        max_parallel: 1,
        fail_fast: true,
        ..ToolSchedulerConfig::default()
    })
    .with_progress(sink);

    let outcome = scheduler
        .execute_batch(
            vec![
                call(0, "call-fail", "failing"),
                call(1, "call-skipped-1", "slow"),
                call(2, "call-skipped-2", "slow"),
            ],
            &tools,
            &ToolContext::new("."),
            None,
        )
        .await;

    let records = records.lock().unwrap();
    for id in ["call-fail", "call-skipped-1", "call-skipped-2"] {
        assert_eq!(
            records
                .iter()
                .filter(|record| record.call_id == id && record.is_start)
                .count(),
            1,
            "one start for {id}"
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.call_id == id && !record.is_start)
                .count(),
            1,
            "one end for {id}"
        );
    }
    for id in ["call-fail", "call-skipped-1", "call-skipped-2"] {
        let end = records
            .iter()
            .find(|record| record.call_id == id && !record.is_start)
            .expect("end recorded");
        assert!(end.is_error, "{id} must end as an error");
        let content = end.content.as_deref().unwrap_or_default();
        if id == "call-fail" {
            assert!(content.contains("boom"), "{id}: {content}");
        } else {
            assert!(content.contains("fail-fast"), "{id}: {content}");
        }
    }
    // The batch report still carries every result in source order.
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call-fail", "call-skipped-1", "call-skipped-2"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aborted_batch_still_emits_start_and_end_for_every_call() {
    let mut tools = ToolRegistry::new();
    let starts = Arc::new(Mutex::new(Vec::new()));
    tools.register(Arc::new(TimedTool {
        name: "slow",
        delay: Duration::from_millis(500),
        starts: Arc::clone(&starts),
    }));
    let (sink, records) = progress_recorder();
    let scheduler = ToolScheduler::new(ToolSchedulerConfig::default()).with_progress(sink);

    let signal = AbortSignal::new();
    signal.abort();
    let mut context = ToolContext::new(".");
    context.signal = Some(signal);

    let outcome = scheduler
        .execute_batch(
            vec![call(0, "call-a", "slow"), call(1, "call-b", "slow")],
            &tools,
            &context,
            None,
        )
        .await;

    assert!(
        starts.lock().unwrap().is_empty(),
        "an aborted batch must not run any tool"
    );
    let records = records.lock().unwrap();
    assert_eq!(records.iter().filter(|record| record.is_start).count(), 2);
    let ends: Vec<&ProgressRecord> = records.iter().filter(|record| !record.is_start).collect();
    assert_eq!(ends.len(), 2, "every aborted call closes with an end");
    for end in &ends {
        assert!(end.is_error);
        assert_eq!(end.content.as_deref(), Some("Operation aborted"));
    }
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call-a", "call-b"]
    );
}
