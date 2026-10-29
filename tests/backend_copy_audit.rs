//! Backend copy/clone audit pins (memory lane, 2026-10-15).
//!
//! The lane's fixes all follow the same recipe — a value was deep-cloned where
//! a borrow, a move, or a lazily built value performs the same job — and every
//! one of them is a differential that a golden literal cannot express. This
//! standalone test binary (never the merged `g_*` harnesses, whose 100+ suites
//! must not pay for a `GlobalAlloc` wrapper) therefore measures with a
//! thread-gated counting allocator and pins each fix against a reference leg:
//!
//! 1. `agent_loop_event_payloads_share_the_transcript` — the loop's event
//!    payloads refcount-bump the run's shared transcript (`AgentStart`/
//!    `TurnStart` contexts and the `AgentEnd` result's context are `Arc`
//!    handles) instead of deep-cloning it, so a run WITH a no-op handler must
//!    not pay a transcript-sized allocation over a handler-less run (the
//!    small message clones behind `TurnEnd`/`StreamEnd`/`AgentEnd` remain,
//!    and the lazy builder still skips them without a handler).
//! 2. `finish_turn_receives_exactly_the_turns_tool_results` — the run-level
//!    `tool_results` tail replaced the per-turn `turn_tool_results` vector
//!    (one fewer deep copy of every tool result per call); the hook must see
//!    this turn's results, in order, and nothing else.
//! 3. `openai_convert_messages_borrowed_replay_is_equivalent_and_cheaper` —
//!    the mid-convo-system collapse only rewrites a transcript that actually
//!    carries a system/developer message; the borrowed replay leg must be
//!    byte-identical to the forced-clone leg and skip the transcript-sized
//!    allocation.
//! 4. `sse_framing_does_not_allocate_per_line` — the shared SSE framing decodes
//!    each line in place out of the accumulation buffer; a chunk of N lines
//!    must cost about one allocation per line (the accumulated `data:` field
//!    it genuinely owns), never one framing copy per line on top.
//! 5. `sse_framing_keeps_lines_and_utf8_across_chunk_boundaries` — the framing
//!    rewrite must keep the delivery semantics: partial lines (including a
//!    multi-byte character split across chunks) stay buffered, CRLF is
//!    stripped, blank lines flush one event block at a time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::agent_loop::{
    run_with_options, AgentEvent, EventStreamFn, FinishTurnAction, FinishTurnContext,
    FinishTurnHook, LoopDeps, LoopOptions,
};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::EventHandler;
use pir::ai::api::openai::{
    build_openai_body, build_openai_body_streamed, convert_messages,
    convert_messages_with_cloned_replay, OpenAiSseParser,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, ModelCost,
    ModelCostRates, OpenAiCompat, StopReason, StreamOptions, TerminalReason, TextContent, ToolCall,
    UserMessage, UserRole,
};

// ---------------------------------------------------------------------------
// Test-only allocation counter (bytes + events, per thread)
// ---------------------------------------------------------------------------
//
// The same shape `tests/tui_frame_reuse.rs` uses: one `GlobalAlloc` wrapper
// around `System` that counts only while a measurement section is open on the
// measuring thread, so the libtest harness, sibling tests (this binary runs
// tests in parallel threads) and the allocator's own TLS reads are unaffected.

struct CountingAllocator;

thread_local! {
    /// Whether this thread is inside a measurement section.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Allocation events seen on this thread since the section opened.
    static ALLOCATION_EVENTS: Cell<u64> = const { Cell::new(0) };
    /// Bytes requested by those allocation events.
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper: delegates straight to System and \
              adds per-thread counters; it holds no unsafe invariant of its \
              own, and the TLS slots are const-initialized so reading them can \
              never allocate (no recursion into the allocator)"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = ALLOCATION_EVENTS.try_with(|events| events.set(events.get() + 1));
                let _ =
                    ALLOCATED_BYTES.try_with(|bytes| bytes.set(bytes.get() + layout.size() as u64));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Allocation cost of one measured section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AllocStats {
    events: u64,
    bytes: u64,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocStats) {
    ALLOCATION_EVENTS.with(|events| events.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = f();
    COUNTING.with(|counting| counting.set(false));
    let stats = ALLOCATION_EVENTS.with(|events| AllocStats {
        events: events.get(),
        bytes: ALLOCATED_BYTES.with(Cell::get),
    });
    (value, stats)
}

async fn measure_async<T>(future: impl Future<Output = T>) -> (T, AllocStats) {
    ALLOCATION_EVENTS.with(|events| events.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = future.await;
    COUNTING.with(|counting| counting.set(false));
    let stats = ALLOCATION_EVENTS.with(|events| AllocStats {
        events: events.get(),
        bytes: ALLOCATED_BYTES.with(Cell::get),
    });
    (value, stats)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn system_message(text: &str) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 2,
        sections: None,
    })
}

fn assistant_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![text_block(text)],
        stop_reason,
        timestamp: 3,
        ..AssistantMessage::default()
    }
}

fn assistant_with_tool_call(call_id: &str, tool_name: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: call_id.into(),
            name: tool_name.into(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        timestamp: 4,
        ..AssistantMessage::default()
    }
}

/// Joined text of a content list (tool-result messages carry one text block).
fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tool that answers with a fixed text and counts its runs.
struct EchoTool {
    runs: Arc<AtomicUsize>,
}

impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "answers with a fixed text"
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("echoed") })
    }
}

// ---------------------------------------------------------------------------
// 1. Event payloads share the transcript (and small payload clones are gated
//    on an installed handler)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn agent_loop_event_payloads_share_the_transcript() {
    // A transcript big enough that one whole-context clone dominates the
    // measurement (~1 MiB text), so the differential is not noise.
    const TRANSCRIPT_BYTES: usize = 1024 * 1024;
    let context = Context {
        system_prompt: Some("system".into()),
        messages: vec![user_message(&"x".repeat(TRANSCRIPT_BYTES))],
        tools: None,
    };
    let deps = || LoopDeps {
        stream: Box::new(|_request| Box::pin(async { assistant_text("done", StopReason::Stop) })),
        ..LoopDeps::default()
    };

    // Leg A: no event handler. The loop still builds the provider request
    // (an `Arc` refcount bump, never a transcript copy), and must NOT pay for
    // any event payload.
    let (without_handler, without_stats) = measure_async(run_with_options(
        deps(),
        context.clone(),
        None,
        LoopOptions::default(),
    ))
    .await;

    // Leg B: a no-op handler. Every event is observable, but the transcript
    // itself is only refcount-bumped (AgentStart/TurnStart contexts and the
    // AgentEnd result's context); only the small per-message payloads are
    // cloned.
    let mut options = LoopOptions::default();
    let no_op_handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    options.events = Some(no_op_handler);
    let (with_handler, with_stats) =
        measure_async(run_with_options(deps(), context.clone(), None, options)).await;

    // Behavior parity: the gating is a pure-clone elision.
    assert_eq!(without_handler.turns, with_handler.turns);
    assert_eq!(without_handler.aborted, with_handler.aborted);
    assert_eq!(
        without_handler.new_messages.len(),
        with_handler.new_messages.len()
    );
    assert_eq!(
        without_handler.terminal.content,
        with_handler.terminal.content
    );
    assert_eq!(
        without_handler.terminal.stop_reason,
        with_handler.terminal.stop_reason
    );

    // Measured: no event payload deep-clones the transcript. The transcript
    // alone is 1 MiB, so any whole-context copy in the handler leg would blow
    // this bound by an order of magnitude; the handler leg only pays the
    // small message/event allocations.
    assert!(
        with_stats.bytes < without_stats.bytes + TRANSCRIPT_BYTES as u64 / 4,
        "event payloads must not deep-clone the transcript: \
         without={without_stats:?} with={with_stats:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. `finish_turn` sees the turn's tool results (run-level tail slice)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn finish_turn_receives_exactly_the_turns_tool_results() {
    let tool_runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool {
        runs: Arc::clone(&tool_runs),
    }));

    let stream_calls = Arc::new(AtomicUsize::new(0));
    let stream: pir::agent::agent_loop::StreamFn = Box::new(move |_request| {
        let index = stream_calls.fetch_add(1, Ordering::SeqCst);
        let message = if index == 0 {
            assistant_with_tool_call("call_1", "echo")
        } else {
            assistant_text("done", StopReason::Stop)
        };
        let future: BoxFuture<'static, AssistantMessage> = Box::pin(async move { message });
        future
    });

    // Turn 1 executes the echo call, turn 2 has no calls: the hook must see
    // exactly one result then none.
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: FinishTurnHook = {
        let seen = Arc::clone(&seen);
        Arc::new(move |context: FinishTurnContext| {
            let texts = context
                .tool_results
                .iter()
                .map(|result| blocks_text(&result.content))
                .collect::<Vec<_>>();
            seen.lock().unwrap().push(texts);
            Box::pin(async { None::<FinishTurnAction> })
        })
    };

    let result = run_with_options(
        LoopDeps {
            stream,
            tools: Arc::new(tools),
            finish_turn: Some(hook),
            max_turns: 4,
            ..LoopDeps::default()
        },
        Context {
            system_prompt: None,
            messages: vec![user_message("go")],
            tools: None,
        },
        None,
        LoopOptions::default(),
    )
    .await;

    assert_eq!(tool_runs.load(Ordering::SeqCst), 1, "the tool ran once");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![vec!["echoed".to_string()], Vec::<String>::new()],
        "each turn's hook input carries exactly that turn's tool results"
    );
    // The run-level aggregate still holds the same result.
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(blocks_text(&result.tool_results[0].content), "echoed");
    assert!(result
        .new_messages
        .iter()
        .any(|message| matches!(message, Message::ToolResult(_))));
}

// ---------------------------------------------------------------------------
// 3. OpenAI `convert_messages`: borrowed replay == cloned replay, cheaper
// ---------------------------------------------------------------------------

#[test]
fn openai_convert_messages_borrowed_replay_is_equivalent_and_cheaper() {
    const TEXT_BYTES: usize = 256 * 1024;
    let model = fake_model();
    let compat = OpenAiCompat::default();
    let context = Context {
        system_prompt: Some("lead".into()),
        messages: vec![user_message(&"y".repeat(TEXT_BYTES))],
        tools: None,
    };

    // Warm any lazily created shared state first (both legs see the same
    // library initialization), then measure the two legs.
    let _ = convert_messages(&model, &context, &compat);
    let (borrowed, borrowed_stats) = measure(|| convert_messages(&model, &context, &compat));
    let (cloned, cloned_stats) =
        measure(|| convert_messages_with_cloned_replay(&model, &context, &compat));

    assert_eq!(
        borrowed, cloned,
        "the borrowed replay leg must be byte-identical to the cloned one"
    );
    assert!(
        cloned_stats.bytes >= borrowed_stats.bytes + (TEXT_BYTES as u64) * 3 / 4,
        "the forced-clone leg must pay the transcript copy: \
         borrowed={borrowed_stats:?} cloned={cloned_stats:?}"
    );

    // Mid-convo system messages flip the collapse branch on: both legs still
    // agree, the collapsed text rides the leading system prompt, and the
    // mid-convo role leaves the wire transcript.
    let collapsing = Context {
        system_prompt: Some("lead".into()),
        messages: vec![
            user_message("one"),
            system_message("mid transcript rule"),
            Message::Assistant(assistant_text("two", StopReason::Stop)),
        ],
        tools: None,
    };
    let collapsed = convert_messages(&model, &collapsing, &compat);
    let collapsed_reference = convert_messages_with_cloned_replay(&model, &collapsing, &compat);
    assert_eq!(collapsed, collapsed_reference);
    assert_eq!(collapsed[0]["role"], serde_json::json!("system"));
    assert_eq!(
        collapsed[0]["content"],
        serde_json::json!("lead\n\nmid transcript rule"),
        "mid-convo system text is folded into the leading system prompt"
    );
    assert!(
        !collapsed
            .iter()
            .any(|message| message["content"] == serde_json::json!("mid transcript rule")),
        "the mid-convo system message itself is dropped"
    );
}

// ---------------------------------------------------------------------------
// 4. SSE framing: no per-line framing allocation
// ---------------------------------------------------------------------------

#[test]
fn sse_framing_does_not_allocate_per_line() {
    const LINES: usize = 512;
    let mut parser = OpenAiSseParser::new(&fake_model());
    let mut events = Vec::new();
    // `data: [DONE]` is the cheapest complete event: it accumulates one
    // `String` per field and dispatches nothing, so the measurement isolates
    // the framing's own per-line cost.
    let chunk = "data: [DONE]\n".repeat(LINES).into_bytes();

    // Warm-up: the framing buffer grows to the chunk's size once; the
    // measured call then reuses that capacity (the buffer is drained, never
    // reallocated).
    parser.feed(&chunk, &mut events).expect("warm-up feed");
    events.clear();
    let ((), stats) = measure(|| parser.feed(&chunk, &mut events).expect("measured feed"));

    assert!(events.is_empty(), "[DONE] dispatches no event");
    // Per line the parser genuinely owns one accumulated `data:` String (plus
    // amortized `Vec` growth); the framing itself must add none. The former
    // per-line `Vec<u8>` copy pushed this to ~2 per line.
    let bound = LINES as u64 + LINES as u64 / 8 + 16;
    assert!(
        stats.events <= bound,
        "framing allocated {} events for {LINES} lines (bound {bound}); \
         expected ~1 (the accumulated field's String) per line",
        stats.events
    );
}

#[test]
fn sse_framing_keeps_lines_and_utf8_across_chunk_boundaries() {
    let mut parser = OpenAiSseParser::new(&fake_model());
    let mut events = Vec::new();
    let first = "data: {\"choices\":[{\"delta\":{\"content\":\"héllo\"}}]}\r\n";
    let bytes = first.as_bytes();
    // Split INSIDE the two-byte `é`: the framing must buffer the partial line
    // (and the partial character) until the rest arrives.
    let split = bytes
        .windows(2)
        .position(|window| window == "é".as_bytes())
        .expect("the fixture carries a two-byte character")
        + 1;
    parser
        .feed(&bytes[..split], &mut events)
        .expect("first half");
    assert!(events.is_empty(), "a partial line must not dispatch");
    parser
        .feed(&bytes[split..], &mut events)
        .expect("second half");
    assert!(
        events.is_empty(),
        "a data field dispatches only at the block's blank line"
    );

    // The CRLF blank line ends the block; the next block is dispatched on its
    // own blank line (lines are never merged across blocks).
    parser.feed(b"\r\n", &mut events).expect("blank line");
    parser
        .feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"bye\"}}]}\n\n",
            &mut events,
        )
        .expect("second block");

    let deltas: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["héllo".to_string(), "bye".to_string()]);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::TextStart { .. })),
        "the first delta opens the text block"
    );
}

// ---------------------------------------------------------------------------
// 6. `message_update` wire: delta-sized, never reply-sized
// ---------------------------------------------------------------------------

/// The per-delta extension payload used to be built as a `serde_json::Value`
/// tree holding TWO copies of the cumulative reply (`to_value(event)` plus a
/// clone of `partial`), then serialized — O(reply) work and bytes per delta,
/// i.e. O(reply²) per reply. The wire now carries the DELTA only (the event
/// without `partial` plus the message's O(1) scalar fields; the JS side
/// accumulates the live message from the deltas), so the same delta against a
/// 1 KiB snapshot and a 64 KiB snapshot must cost nearly the same bytes, while
/// the reference construction (what the old wire paid per delta) grows with the
/// reply — the removed cost.
#[test]
fn message_update_stream_wire_is_delta_sized_not_reply_sized() {
    fn legs(snapshot_bytes: usize) -> (String, AllocStats, AllocStats, String) {
        let partial = assistant_text(&"x".repeat(snapshot_bytes), StopReason::Stop);
        let event = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "chunk".into(),
            partial: Arc::new(partial),
        };
        let mut baselines = pir::agent::agent_loop::ToolArgumentBaselines::new();
        let (wire, wire_stats) = measure(|| {
            pir::extensions::runtime::message_update_stream_json(&event, 1, &mut baselines)
        });
        // The reference construction plus the serialization the dispatcher used
        // to perform on the actor thread (the old per-delta cost).
        let (_, reference_stats) = measure(|| {
            let payload = pir::agent::agent_loop::message_update_payload(&event);
            serde_json::to_string(&payload).expect("reference payload serializes")
        });
        // The fallback snapshot (materialized only for a dispatch the JS
        // accumulator cannot apply exactly) is the reference `message` value.
        let snapshot = pir::extensions::runtime::message_update_snapshot_json(&event);
        (wire, wire_stats, reference_stats, snapshot)
    }

    let (small_wire, small_wire_stats, small_reference_stats, _) = legs(1_024);
    let (large_wire, large_wire_stats, large_reference_stats, large_snapshot) = legs(65_536);

    // The wire never carries the accumulated content.
    assert!(
        !large_wire.contains(&"x".repeat(64)),
        "the wire must not carry the snapshot: {} bytes",
        large_wire.len()
    );
    // Delta-sized: a 64x larger snapshot must not move the wire.
    assert!(
        large_wire.len() <= small_wire.len() + 256,
        "the wire must stay delta-sized: small={} bytes large={} bytes",
        small_wire.len(),
        large_wire.len()
    );
    assert!(
        large_wire_stats.bytes <= small_wire_stats.bytes + 1_024,
        "the wire serializer must stay delta-sized: small={small_wire_stats:?} \
         large={large_wire_stats:?}"
    );
    // The reference construction IS reply-sized (that is the cost this lane
    // removed: ~64x here, asserted above an 8x lower bound).
    assert!(
        large_reference_stats.bytes > small_reference_stats.bytes * 8,
        "the reference construction must grow with the reply: small={small_reference_stats:?} \
         large={large_reference_stats:?}"
    );
    // The fallback snapshot is the reference `message` (the partial itself).
    let snapshot: serde_json::Value =
        serde_json::from_str(&large_snapshot).expect("snapshot parses");
    assert_eq!(
        snapshot["role"],
        serde_json::json!("assistant"),
        "the fallback snapshot is the serialized partial"
    );
    assert_eq!(
        snapshot["content"],
        serde_json::json!([text_block(&"x".repeat(65_536))]),
        "the fallback snapshot carries the accumulated content"
    );
    assert_eq!(snapshot["timestamp"], serde_json::json!(3));
}

/// The tool-call argument leg: the provider mirrors the PARSED in-flight JSON
/// into the block (`openai/sse.rs`), so a 32-byte raw fragment used to cost the
/// WHOLE accumulated arguments object per delta — O(args) bytes per delta and
/// O(args^2) per call (measured: a 3000-fragment, 72 KB argument stream peaked
/// at 100 MB vs 33 MB for the same stream with no handler). The host now ships
/// the O(delta) `patch` (`arguments[key] += tail`) whenever the change is
/// exactly one string argument gaining a suffix, and the whole block only when
/// it is not.
#[test]
fn message_update_tool_argument_wire_is_patch_sized_not_block_sized() {
    fn tool_call_partial(argument_bytes: usize, tail_bytes: usize) -> Arc<AssistantMessage> {
        let mut arguments = BTreeMap::new();
        arguments.insert(
            "file".to_string(),
            serde_json::json!("x".repeat(argument_bytes) + &"y".repeat(tail_bytes)),
        );
        Arc::new(AssistantMessage {
            role: pir::ai::types::AssistantRole::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "write".into(),
                arguments,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: pir::ai::types::Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            provider_thinking_level: None,
            thinking_level: None,
            duration_ms: None,
            timestamp: 0,
        })
    }

    /// (patch wire bytes for one 32-byte fragment, block wire bytes for the
    /// same fragment when it cannot be patched).
    ///
    /// The patch PRECONDITION is that the single changed value EXTENDS the old
    /// one (`tool_argument_patch`: one string key whose new value starts with
    /// the value the JS side already holds, every other key deep-equal). The
    /// fixture therefore builds the second value as a strict append of the
    /// first: `"x"*N` then `"x"*N + "y"*32`, with the fragment being exactly
    /// those 32 `y`s. A fixture that grew a PREFIX instead (`"x"*(N+32) +
    /// "y"*32`) would not be a prefix extension at all — the patch correctly
    /// refuses it and the leg would measure the block route twice.
    fn legs(argument_bytes: usize) -> (usize, usize) {
        const TAIL: usize = 32;
        let mut baselines = pir::agent::agent_loop::ToolArgumentBaselines::new();
        // The block route: no baseline for the index (its first delta).
        let first = AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: "y".repeat(TAIL),
            partial: tool_call_partial(argument_bytes, 0),
        };
        let block_wire =
            pir::extensions::runtime::message_update_stream_json(&first, 1, &mut baselines);
        // The patch route: the strict append of those 32 bytes against the
        // baseline the first event just recorded.
        let second = AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: "y".repeat(TAIL),
            partial: tool_call_partial(argument_bytes, TAIL),
        };
        let patch_wire =
            pir::extensions::runtime::message_update_stream_json(&second, 1, &mut baselines);
        (patch_wire.len(), block_wire.len())
    }

    let (small_patch, small_block) = legs(1_024);
    let (large_patch, large_block) = legs(65_536);

    // The patch is delta-sized and flat: 64x the arguments must not move it.
    assert!(
        large_patch <= small_patch + 64,
        "the arguments patch must stay delta-sized: small={small_patch} large={large_patch}"
    );
    assert!(
        large_patch <= small_patch * 2,
        "the arguments patch must not grow with the arguments:          small={small_patch} large={large_patch}"
    );
    // The block fallback IS arguments-sized (the cost the patch removes for the
    // growing-argument case), asserted above an 8x lower bound.
    assert!(
        large_block > small_block * 8,
        "the block fallback must grow with the arguments: small={small_block} large={large_block}"
    );
    assert!(
        large_patch * 8 < large_block,
        "a patchable fragment must cost far less than the block:          patch={large_patch} block={large_block}"
    );
}

// ---------------------------------------------------------------------------
// 7. The per-delta `message_update` cost is flat for every reading shape
// ---------------------------------------------------------------------------

/// The per-delta `message_update` cost must be delta-sized for EVERY handler
/// shape that does not itself stringify the message: the no-op handler, the
/// pi-phone shape (reads `message.role` / `message.timestamp` and forwards the
/// delta), and a handler that reads the accumulated `content` on every delta.
///
/// All three used to pay O(reply) per delta — the host serialized the
/// cumulative snapshot per dispatch (the read-content leg measured ~770 MB over
/// a 10 000-delta reasoning stream; the no-op leg ~709 MB) — and the O(reply²)
/// net is what let one defensive handler dominate a whole run. Now the host
/// ships the delta only and the JS side accumulates the live message
/// (`prelude.js.txt`), so what crosses the boundary per dispatch is the delta
/// plus the message's O(1) fields, and the snapshot fallback never fires.
///
/// This test pins, per shape and per reply size (4x apart):
/// - one dispatch per streamed event, and the handler observed every one;
/// - ZERO fallback snapshot materializations;
/// - the exact bytes the JS entry receives (probe on the dispatch entry) stay
///   flat as the reply grows, and below a delta-sized mean bound;
/// - the loop thread's own per-event gross bytes (counting allocator, the same
///   thread-scoped series as the sibling pins) stay flat and small.
///
/// The bounds are derived from the mechanism, not from a recorded run (this
/// lane may not execute tests): a 64-byte delta plus the message scalars
/// (`role`, `api`, `provider`, `model`, usage object, `stopReason`, `timestamp`
/// — well under 1 KiB) leaves a 4 KiB mean bound, and the 2x + 256 byte
/// flatness slack absorbs the stream's two non-delta events. The previous
/// construction grew linearly with the reply — ~4x the mean for 4x the deltas —
/// which no such bound admits. The stringify-a-growing-message leg is NOT
/// pinned: that cost is the handler's own choice, not pir state.
#[tokio::test(flavor = "current_thread")]
async fn message_update_per_delta_cost_is_flat_for_every_reading_shape() {
    const DELTA_BYTES: usize = 64;
    const SHORT_DELTAS: usize = 128;
    const LONG_DELTAS: usize = 512;
    /// Mean bytes the JS dispatch entry receives per `message_update`.
    const MEAN_WIRE_BOUND: u64 = 4 * 1024;
    /// Gross bytes per delta on the stream thread (mean over the run).
    const MEAN_LOOP_BOUND: u64 = 8 * 1024;

    // The three handler shapes, plus the probes: the dispatch entry (wire
    // bytes in, one count per dispatch) and the fallback snapshot host fn.
    // `__updates` proves the handler ran once per dispatch.
    let fixtures = [
        (
            "noop",
            r#"
            pi.on("message_update", () => { __updates += 1; });
            "#,
            r#"({ updates: __updates, role: null, text: null })"#,
        ),
        (
            // pi-phone's real shape: `if (event.message?.role === "assistant")
            // last = event.message.timestamp`, then the slim envelope. The
            // terminal event is skipped because the reference payload's
            // `message` for it is the historical `{role: "assistant"}` stub
            // (no timestamp) — a pre-existing shape, not this lane's.
            "read-role",
            r#"
            pi.on("message_update", (event) => {
                __updates += 1;
                const type = event.assistantMessageEvent ? event.assistantMessageEvent.type : null;
                if (type === "done" || type === "error") return;
                if (event.message && event.message.role === "assistant") {
                    __stamp = event.message.timestamp;
                }
            });
            "#,
            r#"({ updates: __updates, role: __stamp, text: null })"#,
        ),
        (
            "read-content",
            r#"
            pi.on("message_update", (event) => {
                __updates += 1;
                const blocks = event.message ? event.message.content : null;
                // The stream's `start` carries an EMPTY content (the first
                // block is opened by a chunk's `text_start`): read the length
                // through optional chaining so the probe value stays total.
                if (Array.isArray(blocks) && blocks[0]?.text) __text = blocks[0].text.length;
            });
            "#,
            r#"({ updates: __updates, role: null, text: __text })"#,
        ),
    ];

    /// Run `deltas` streamed deltas through the loop with the fixture installed
    /// and return (per-event gross bytes on this thread, run result, probe).
    async fn run_measured(
        runtime: &pir::extensions::ExtensionHandle,
        deltas: usize,
        probe_expression: &str,
    ) -> (Vec<u64>, AssistantMessage, serde_json::Value) {
        // The provider's cumulative snapshots are built OUTSIDE the measured
        // section (a real adapter pays for them off the loop too); the producer
        // task only moves pre-built events.
        let mut events = Vec::with_capacity(deltas + 4);
        // A real adapter pushes `start` BEFORE the provider's first chunk (the
        // streaming path snapshots the parser before reading the response
        // body), so its content is EMPTY: the first content block is opened by
        // the chunk's own `text_start`. A `start` already carrying a block
        // would make that `text_start` a NON-dense block opening (index 0 into
        // an accumulator that already holds one block) — the JS accumulator's
        // loud fallback, which is exactly what the `fallbacks: 0` assertion
        // rules out: these legs must measure the healthy per-delta path.
        let mut start_partial = assistant_text("", StopReason::Stop);
        start_partial.content.clear();
        events.push(AssistantMessageEvent::Start {
            partial: Arc::new(start_partial),
        });
        events.push(AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(assistant_text("", StopReason::Stop)),
        });
        let mut accumulated = String::new();
        let chunk = "d".repeat(DELTA_BYTES);
        for _ in 0..deltas {
            accumulated.push_str(&chunk);
            events.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: chunk.clone(),
                partial: Arc::new(assistant_text(&accumulated, StopReason::Stop)),
            });
        }
        events.push(AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: accumulated.clone(),
            partial: Arc::new(assistant_text(&accumulated, StopReason::Stop)),
        });
        events.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant_text(&accumulated, StopReason::Stop),
        ));

        let stream_events = Arc::new(Mutex::new(Some(events)));
        let stream: EventStreamFn = Box::new(move |_request| {
            let events = stream_events.lock().unwrap().take();
            Box::pin(async move {
                let stream = AssistantMessageEventStream::new();
                let sink = stream.clone();
                tokio::spawn(async move {
                    if let Some(events) = events {
                        for event in events {
                            sink.push(event);
                        }
                    }
                });
                stream
            })
        });

        // Per-`StreamEvent` gross allocations on THIS thread: the cumulative
        // counter is sampled at every callback and differenced. The sample
        // buffer is pre-reserved so its own growth is not part of the series.
        let samples: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(deltas + 8)));
        let samples_for_handler = Arc::clone(&samples);
        let last = Arc::new(AtomicUsize::new(0));
        let last_for_handler = Arc::clone(&last);
        let handler: EventHandler = Arc::new(move |event: AgentEvent| {
            if matches!(event, AgentEvent::StreamEvent { .. }) {
                let cumulative = ALLOCATED_BYTES.with(Cell::get);
                let previous = last_for_handler.swap(cumulative as usize, Ordering::SeqCst) as u64;
                samples_for_handler
                    .lock()
                    .unwrap()
                    .push(cumulative.saturating_sub(previous));
            }
        });
        let options = LoopOptions {
            event_stream: Some(stream),
            events: Some(handler),
            ..LoopOptions::default()
        };
        let deps = LoopDeps {
            extensions: Some(runtime.clone()),
            max_turns: 1,
            ..LoopDeps::default()
        };

        ALLOCATION_EVENTS.with(|events| events.set(0));
        ALLOCATED_BYTES.with(|bytes| bytes.set(0));
        COUNTING.with(|counting| counting.set(true));
        let result = run_with_options(deps, Context::default(), None, options).await;
        COUNTING.with(|counting| counting.set(false));

        // The dispatches are queued, not awaited per delta: drain them before
        // probing (the loop does this itself before `message_end`; here the run
        // already returned, so the actor can still be finishing the last
        // dispatches — `wait_idle` is the runtime's own quiescence drain).
        let probe = runtime.eval_json(probe_expression).await.expect("probe");
        let samples = samples.lock().unwrap().clone();
        (samples, result.terminal, probe)
    }

    for (name, handler_source, probe_expression) in fixtures {
        let temp = std::env::temp_dir().join(format!(
            "pir-alloc-message-update-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&temp).expect("create temp dir");
        let path = temp.join("ext.ts");
        std::fs::write(
            &path,
            format!(
                r#"
                export default function (pi) {{
                    globalThis.__updates = 0;
                    globalThis.__stamp = null;
                    globalThis.__text = null;
                    globalThis.__wireBytes = 0;
                    globalThis.__wireCalls = 0;
                    globalThis.__snapshotCalls = typeof __pir_host.messageUpdateSnapshotJson;
                    const entry = globalThis.__pir_dispatchEventReadOnly;
                    globalThis.__pir_dispatchEventReadOnly = async function (eventName, payloadJson) {{
                        if (eventName === "message_update") {{
                            globalThis.__wireCalls += 1;
                            globalThis.__wireBytes += payloadJson.length;
                        }}
                        return entry.call(globalThis, eventName, payloadJson);
                    }};
                    {handler_source}
                }}
                "#
            ),
        )
        .expect("write fixture");
        let runtime = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
            cwd: temp.clone(),
            mode: "headless".to_string(),
            has_ui: false,
            model: None,
            session_id: None,
            system_prompt: String::new(),
            is_idle: true,
            limits: Some(Default::default()),
        })
        .expect("runtime");
        let loaded = runtime
            .load(std::slice::from_ref(&path))
            .await
            .expect("load");
        assert!(loaded.errors.is_empty(), "{name}: {:?}", loaded.errors);

        // `globalThis.__pir_messageUpdateFallbacks` (never a bare
        // `__pir_messageUpdateFallbacks`): the property is created by the FIRST
        // fallback, so a stream with none (the healthy path these legs pin)
        // leaves it undeclared — and a bare reference to an undeclared global
        // throws a ReferenceError, which rquickjs surfaces as "Exception
        // generated by QuickJS" for the whole probe. Property access on
        // `globalThis` is total; `?? 0` then yields the zero the healthy path
        // expects.
        let probe_expression = format!(
            "JSON.stringify(Object.assign({{ wireBytes: __wireBytes, wireCalls: __wireCalls, \
             snapshotCalls: __snapshotCalls, \
             fallbacks: (globalThis.__pir_messageUpdateFallbacks ?? 0) }}, \
             {probe_expression}))"
        );
        let (short, short_terminal, short_probe) =
            run_measured(&runtime, SHORT_DELTAS, &probe_expression).await;
        let (long, long_terminal, long_probe) =
            run_measured(&runtime, LONG_DELTAS, &probe_expression).await;

        // The runs are real: every stream event reached the Rust-side consumer
        // (and so the dispatcher), and the stream completed.
        assert_eq!(
            short.len(),
            SHORT_DELTAS + 4,
            "{name}: one sample per event"
        );
        assert_eq!(long.len(), LONG_DELTAS + 4, "{name}: one sample per event");
        assert_eq!(
            long_terminal.content,
            vec![text_block(&"d".repeat(DELTA_BYTES * LONG_DELTAS))],
            "{name}: the stream completed"
        );
        assert_eq!(short_terminal.stop_reason, StopReason::Stop);

        // One dispatch per streamed event, and the handler ran for each. The
        // dispatch-entry wrapper and `__updates` are RUNTIME-level counters and
        // both legs share this fixture's runtime, so the expectation is
        // CUMULATIVE (`long == short + LONG_DELTAS + 4`): the assertion still
        // rejects any extra or missing dispatch (and any coalescing) in either
        // leg, it just accounts for the counters never being reset between
        // legs.
        let mut cumulative_dispatches = 0u64;
        for (label, probe, deltas) in [
            ("short", &short_probe, SHORT_DELTAS),
            ("long", &long_probe, LONG_DELTAS),
        ] {
            cumulative_dispatches += deltas as u64 + 4;
            assert_eq!(
                probe["wireCalls"].as_u64(),
                Some(cumulative_dispatches),
                "{name}/{label}: one dispatch per stream event, accumulated over the shared \
                 runtime: {probe:?}"
            );
            assert_eq!(
                probe["updates"].as_u64(),
                Some(cumulative_dispatches),
                "{name}/{label}: the handler observed every dispatch, accumulated over the shared \
                 runtime: {probe:?}"
            );
            assert_eq!(
                probe["snapshotCalls"].as_str(),
                Some("undefined"),
                "{name}/{label}: the snapshot host function is gone (the host classifies the wire): {probe:?}"
            );
            assert_eq!(
                probe["fallbacks"].as_u64(),
                Some(0),
                "{name}/{label}: the accumulator applied every delta exactly: {probe:?}"
            );
        }
        // The reading shapes saw the accumulated message, not a stub.
        if name == "read-role" {
            assert_eq!(
                short_probe["role"].as_u64(),
                Some(3),
                "the pi-phone shape reads the real timestamp: {short_probe:?}"
            );
            assert_eq!(long_probe["role"].as_u64(), Some(3));
        }
        if name == "read-content" {
            assert_eq!(
                short_probe["text"].as_u64(),
                Some((DELTA_BYTES * SHORT_DELTAS) as u64),
                "the read-content shape sees the accumulated text: {short_probe:?}"
            );
            assert_eq!(
                long_probe["text"].as_u64(),
                Some((DELTA_BYTES * LONG_DELTAS) as u64)
            );
        }

        // Per-dispatch bytes crossing the boundary: delta-sized, flat.
        let wire_per_dispatch = |probe: &serde_json::Value| {
            probe["wireBytes"].as_u64().expect("wire bytes")
                / probe["wireCalls"].as_u64().expect("wire calls").max(1)
        };
        let short_wire_mean = wire_per_dispatch(&short_probe);
        let long_wire_mean = wire_per_dispatch(&long_probe);
        assert!(
            long_wire_mean <= MEAN_WIRE_BOUND,
            "{name}: a {DELTA_BYTES}-byte delta must stay below {MEAN_WIRE_BOUND} bytes \
             across the boundary: long_mean={long_wire_mean} probe={long_probe:?}"
        );
        // Flat, not O(reply): 4x the reply must not grow the per-delta bytes.
        // (The read-content leg is the one that used to be O(reply) per delta.)
        assert!(
            long_wire_mean <= short_wire_mean * 2 + 256,
            "{name}: per-delta wire bytes must not grow with the reply: \
             short_mean={short_wire_mean} long_mean={long_wire_mean}"
        );

        // The loop thread's own share stays flat and small (unchanged from the
        // previous lane: the loop only queues the event).
        let mean = |samples: &[u64]| samples.iter().sum::<u64>() / samples.len() as u64;
        let short_mean = mean(&short);
        let long_mean = mean(&long);
        assert!(
            long_mean <= MEAN_LOOP_BOUND,
            "{name}: a {DELTA_BYTES}-byte delta must stay far below {MEAN_LOOP_BOUND} \
             gross bytes on the stream thread: long_mean={long_mean}"
        );
        assert!(
            long_mean <= short_mean * 2 + 1024,
            "{name}: the loop's per-delta cost must not grow with the reply: \
             short_mean={short_mean} long_mean={long_mean}"
        );

        let _ = std::fs::remove_dir_all(&temp);
    }
}

// ---------------------------------------------------------------------------
// 4. OpenAI streamed body: the single-text message is never copied
// ---------------------------------------------------------------------------

/// INVARIANT (no text copy): a one-message request body must not copy that
/// message's text while BUILDING the body — only the serialized request body
/// itself holds a copy. The streamed body emits a single-text message from the
/// borrow (`WireItem::Text`), so its total allocation is ~one text; the
/// value-based body additionally builds the part `Value` and the item `Value`
/// (two more copies). Byte-identity of the two bodies is pinned in
/// `tests/ai_request_snapshots.rs`.
#[test]
fn openai_streamed_body_does_not_copy_the_message_text() {
    const TEXT_BYTES: usize = 256 * 1024;
    let model = fake_model();
    let compat = OpenAiCompat::default();
    let context = Context {
        system_prompt: None,
        messages: vec![user_message(&"z".repeat(TEXT_BYTES))],
        tools: None,
    };
    let options = StreamOptions::default();
    let (streamed_bytes, streamed_stats) = measure(|| {
        let body =
            build_openai_body_streamed(&model, &context, &options, &compat).expect("streamed body");
        serde_json::to_vec(&body).expect("serialize streamed")
    });
    let (value_bytes, value_stats) = measure(|| {
        let body = build_openai_body(&model, &context, &options, &compat).expect("value body");
        serde_json::to_vec(&body).expect("serialize value")
    });
    assert_eq!(
        streamed_bytes, value_bytes,
        "the streamed and value bodies must serialize identically"
    );
    assert!(
        value_stats.bytes >= streamed_stats.bytes + TEXT_BYTES as u64,
        "the value path must copy the text at least once more than the streamed          path: streamed={streamed_stats:?} value={value_stats:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. P1 performance lane: single-pass CBOR encode, direct session-line parse,
//    in-place SSE batch split
// ---------------------------------------------------------------------------

/// CBOR encoder: the former shape walked the whole value twice (validate,
/// then write) and allocated a `HashSet` per object for a duplicate-key check
/// `serde_json::Value` cannot trigger. One fused traversal must allocate only
/// the output buffer's amortized growth, and produce identical bytes.
#[test]
fn cbor_encode_validates_while_it_writes() {
    let mut entries = Vec::new();
    for index in 0..256u64 {
        let mut object = serde_json::Map::new();
        object.insert(
            "id".to_string(),
            serde_json::json!(format!("entry-{index}")),
        );
        object.insert("phase".to_string(), serde_json::json!("turn"));
        object.insert("count".to_string(), serde_json::json!(index));
        object.insert("payload".to_string(), serde_json::json!("x".repeat(64)));
        entries.push(serde_json::Value::Object(object));
    }
    let value = serde_json::json!({
        "t": "state",
        "m": "snapshot",
        "as": "ops",
        "ops": entries,
    });
    let options = pir::protocol::CborOptions::default();
    let expected = pir::protocol::encode_cbor(&value, options).expect("baseline encode");
    let (decoded, stats) = measure(|| {
        let encoded = pir::protocol::encode_cbor(&value, options).expect("measured encode");
        assert_eq!(encoded, expected, "encoded bytes must not change");
        encoded
    });
    eprintln!("[perf-counter] cbor_encode_256_objects: {stats:?}");
    assert!(!decoded.is_empty());
    // One output `Vec` growth path is all the encoder should allocate. The
    // old shape added one `HashSet` per object: measured 270 allocation
    // events before the single-pass rewrite, 13 after.
    assert!(
        stats.events <= 32,
        "CBOR encode allocated {} events ({} bytes); expected only the output \
         buffer growth, got a per-object allocation",
        stats.events,
        stats.bytes
    );
}

fn p1_user_message(index: u64) -> pir::session_manager::StoredMessage {
    pir::session_manager::StoredMessage::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: format!("replay message {index:04}: the quick brown fox jumps over the lazy dog"),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: index,
    })
}

/// Session open: a valid JSONL line now deserializes STRAIGHT into
/// `FileEntry`; the former shape materialized each line as a
/// `serde_json::Value` and converted that tree afterwards. Measured 7906
/// allocation events for 128 lines before, 4302 after; the pinned bound
/// catches a regression to the Value intermediate while leaving headroom for
/// unrelated small changes.
#[test]
fn session_entry_parse_skips_the_value_intermediate() {
    use pir::session_manager::{FileEntry, MessageEntry, SessionManager};

    const LINES: u64 = 128;
    let mut body = String::from(
        "{\"type\":\"session\",\"version\":3,\"id\":\"11111111-2222-3333-4444-555555555555\",\
         \"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"C:\\\\repo\"}\n",
    );
    for index in 0..LINES {
        let entry = FileEntry::Message(MessageEntry {
            id: format!("entry-{index:04}"),
            parent_id: (index > 0).then(|| format!("entry-{:04}", index - 1)),
            timestamp: format!("2025-01-01T00:00:{:02}.000Z", index % 60),
            message: p1_user_message(index),
        });
        body.push_str(&serde_json::to_string(&entry).expect("serialize fixture line"));
        body.push('\n');
    }
    let dir = std::env::temp_dir().join(format!("pir-p1-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("replay.jsonl");
    std::fs::write(&path, &body).expect("write fixture session");

    // Warm-up (untimed): the first open builds process-wide state; the
    // measured open is the deterministic replay cost.
    drop(SessionManager::open(&path).expect("warm-up open"));
    let (session, stats) = measure(|| SessionManager::open(&path).expect("measured open"));
    eprintln!("[perf-counter] session_open_128_entries: {stats:?}");
    assert_eq!(session.entries().len(), LINES as usize);
    drop(session);
    let _ = std::fs::remove_dir_all(&dir);

    let bound = 5_200;
    assert!(
        stats.events <= bound,
        "session open allocated {} events for {LINES} lines (bound {bound}); \
         a per-line serde_json::Value intermediate is back",
        stats.events
    );
}

const P1_GOOGLE_SSE: &str = r#"data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Think"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":0,"thoughtsTokenCount":1,"totalTokenCount":11}}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"ing hard","thoughtSignature":"AAAAAAAAAAAAAAAAAAAAAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":"Hello "}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":"world","thoughtSignature":"BBBBBBBBBBBBBBBBBBBBAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}},"thoughtSignature":"CCCCCCCCCCCCCCCCCCCCAA=="}]}}]}

data: {"responseId":"resp-1","candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":9,"thoughtsTokenCount":3,"cachedContentTokenCount":2,"totalTokenCount":24}}

"#;

/// Batch SSE split: one in-place scan of the body instead of a normalized
/// whole-body copy plus a temporary `Vec<&str>` per event block. The wire
/// behavior must be unchanged (this is the same fixture the
/// `sse_google_parse_fixture` bench parses), and CRLF / multi-line `data:` /
/// non-data fields / a missing trailing blank line must all frame identically.
#[test]
fn google_sse_batch_parse_and_split_rules() {
    use pir::ai::types::AssistantMessageEvent;

    let model = fake_model();
    let body = P1_GOOGLE_SSE.as_bytes();
    let expected_len = {
        let probe = pir::ai::api::google::parse_google_sse(body, &model).expect("fixture parses");
        assert!(!probe.is_empty());
        probe.len()
    };
    let (events, stats) =
        measure(|| pir::ai::api::google::parse_google_sse(body, &model).expect("measured parse"));
    eprintln!("[perf-counter] google_sse_parse_fixture: {stats:?}");
    assert_eq!(events.len(), expected_len);
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    // The final chunk's empty `text` part still reports an (empty) delta.
    assert_eq!(deltas, vec!["Hello ", "world", ""]);

    // Framing rules: LF, CRLF, no trailing blank line, ignored non-data
    // fields, a JSON payload split across two `data:` lines, and leading /
    // trailing blank lines all yield the same single text delta.
    let payload = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello "}]},"finishReason":"STOP"}]}"#;
    let split = payload.find("\"candidates\"").expect("split point");
    let variants = [
        format!("data: {payload}\n\n"),
        format!("data: {payload}\r\n\r\n"),
        format!("data: {payload}"),
        format!("event: message\r\nid: 7\r\ndata: {payload}\r\n\r\n"),
        format!(
            "data: {}\r\ndata: {}\r\n\r\n",
            &payload[..split],
            &payload[split..]
        ),
        format!("\r\ndata: {payload}\r\n\r\n\r\n"),
    ];
    for (index, variant) in variants.iter().enumerate() {
        let events = pir::ai::api::google::parse_google_sse(variant.as_bytes(), &model)
            .unwrap_or_else(|error| panic!("variant {index} must parse: {error}"));
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hello "], "variant {index}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::Done { .. }))
                .count(),
            1,
            "variant {index} must end with one Done event"
        );
    }
}

// ---------------------------------------------------------------------------
// Env client copy audit
// ---------------------------------------------------------------------------

/// `TextLineBuffer` advances a cursor instead of rebuilding the remainder per
/// line: taking 1500 lines out of two 64 KiB chunks moves at most one chunk's
/// worth of bytes (the amortized compaction), where the old
/// `buffered = buffered[newline + 1..].to_string()` moved the whole remainder
/// once per line (tens of megabytes for this input).
#[test]
fn text_line_buffer_advances_a_cursor_instead_of_copying_per_line() {
    use pir::env_client::remote_env::TextLineBuffer;

    let line = format!("{}\n", "x".repeat(63));
    let chunk = line.repeat(1024);
    assert_eq!(chunk.len(), 64 * 1024);

    let mut buffer = TextLineBuffer::new();
    buffer.push_str(&chunk);
    buffer.push_str(&chunk);
    let mut lines = 0;
    for _ in 0..1500 {
        let line = buffer.next_line().expect("a complete line");
        assert_eq!(line.len(), 63);
        lines += 1;
    }
    assert_eq!(lines, 1500);
    assert!(
        buffer.compacted_bytes() <= chunk.len(),
        "consuming 1500 lines moved {} bytes; the amortized bound is one chunk ({})",
        buffer.compacted_bytes(),
        chunk.len()
    );
}

/// `StreamParser` consumes frames with a cursor: 5000 small frames move at
/// most one live buffer's worth of bytes through compaction, not the
/// remaining-buffer copy per frame that made a burst of small frames
/// quadratic.
#[test]
fn stream_parser_bytes_moved_are_linear_in_chunk_count() {
    use pir::env_client::connection::StreamParser;
    use pir::env_daemon::frame::{self, Frame, RESULT};

    let mut parser = StreamParser::new("token");
    let synced = parser
        .feed(b"PI-ENV token\n", &mut |_| {})
        .expect("the sync line is accepted");
    assert!(synced.is_empty());

    let mut frame_count = 0;
    for index in 0..5000u32 {
        let mut bytes = Vec::new();
        frame::write_frame(
            &mut bytes,
            &Frame::new(RESULT, index, serde_json::json!({ "i": index })),
        )
        .expect("encode frame");
        frame_count += parser
            .feed(&bytes, &mut |_| {})
            .expect("a valid frame")
            .len();
    }
    assert_eq!(frame_count, 5000);
    assert!(
        parser.compacted_bytes() <= 128 * 1024,
        "5000 frames moved {} bytes through compaction (linear budget: 128 KiB)",
        parser.compacted_bytes()
    );
}
