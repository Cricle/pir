//! Memory-lane suite: bounded (chunked) summarization and tail-first session
//! paging.
//!
//! Pins the mechanisms that keep a very large session bounded:
//!
//! - the chunk splitter's boundary rules (turn-boundary cut, trailing
//!   tool-result carry, an indivisible oversize message) and its exact
//!   serialized weight math;
//! - the streaming chunker (the paged path) producing the SAME chunks as the
//!   resident splitter;
//! - a chunked summarization run whose EVERY request stays inside the model
//!   window budget (the acceptance contract the big-session probe measures);
//! - the tail-first page open (counters prove only the tail was parsed), full
//!   append fidelity through a paged open, widening, and the on-disk
//!   interop of a paged compaction.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    TextContent, ToolCall, ToolResultMessage, ToolResultRole, Usage, UserMessage, UserRole,
};
use pir::compaction::chunking::{split_summarization_chunks, summarization_chunk_budget};
use pir::compaction::prompts::TURN_PREFIX_SUMMARIZATION_PROMPT;
use pir::compaction::{
    compact_paged, prepare_compaction_refs, serialize_conversation,
    summarize_messages_chunked_span, CompactionConfig, CompleteFn, ProjectionEntryMeta,
    SummarizationStyle,
};
use pir::compaction::{SpanFraming, StreamingChunker};
use pir::session_manager::projection::SessionProjection;
use pir::session_manager::{
    FileEntry, SessionManager, SessionTailPage, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_msg(text: &str, usage_total: u64) -> Message {
    Message::Assistant(AssistantMessage {
        api: "openai-completions".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content: vec![text_block(text)],
        stop_reason: StopReason::Stop,
        usage: Usage {
            total_tokens: usage_total,
            ..Default::default()
        },
        timestamp: 2,
        ..Default::default()
    })
}

fn system_msg(text: &str) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_with_tool_call(id: &str) -> Message {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    Message::Assistant(AssistantMessage {
        api: "openai-completions".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            total_tokens: 10,
            ..Default::default()
        },
        timestamp: 2,
        ..Default::default()
    })
}

fn tool_result(id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content: vec![text_block(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 3,
    })
}

fn test_model(context_window: u64) -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock".into(),
        api: "openai-completions".into(),
        provider: "mock".into(),
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
        context_window,
        max_tokens: 4096,
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

/// A summarizer that records every prompt it was handed and answers with a
/// short fixed summary.
fn recording_summarizer(prompts: Arc<Mutex<Vec<String>>>) -> Arc<CompleteFn> {
    Arc::new(Box::new(move |context: Context| {
        let prompts = Arc::clone(&prompts);
        Box::pin(async move {
            let text = context
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User(user) => Some(
                        user.content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            prompts.lock().expect("prompt log").push(text);
            Ok(AssistantMessage {
                api: "openai-completions".into(),
                provider: "mock".into(),
                model: "mock-model".into(),
                content: vec![text_block("SUMMARY")],
                stop_reason: StopReason::Stop,
                usage: Usage {
                    total_tokens: 5,
                    ..Default::default()
                },
                timestamp: 4,
                ..Default::default()
            })
        })
    }))
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-chunking-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write a v3 JSONL session with `pairs` user/assistant exchanges, ~`filler`
/// characters per message, and a growing usage on every assistant message.
fn write_session(path: &Path, pairs: usize, filler: usize) {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path).expect("create session");
    let payload = "x".repeat(filler);
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "11111111",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "C:\\\\repo",
        })
    )
    .expect("header");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "model_change", "id": "m0", "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "mock", "modelId": "mock-model",
        })
    )
    .expect("model change");
    let mut parent = "m0".to_owned();
    for index in 0..pairs {
        let user_id = format!("u{index:06}");
        let assistant_id = format!("a{index:06}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": user_id, "parentId": parent,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "user", "timestamp": 0,
                    "content": [{"type": "text", "text": format!("question {index}: {payload}")}],
                },
            })
        )
        .expect("user line");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": assistant_id, "parentId": user_id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "assistant", "timestamp": 0,
                    "api": "openai-completions", "provider": "mock", "model": "mock-model",
                    "stopReason": "stop",
                    "usage": {
                        "input": 1_500_000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 1_500_100,
                        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                    },
                    "content": [{"type": "text", "text": format!("answer {index}: {payload}")}],
                },
            })
        )
        .expect("assistant line");
        parent = assistant_id;
    }
}

/// Write a v3 JSONL session whose EARLY PREFIX holds a real-shape
/// `compaction` entry (summary ~`summary_bytes` long), followed by `pairs`
/// ordinary exchanges that push the entry out of a small tail page.
///
/// Returns the summary text (with its unique marker) so the test can prove it
/// reached the summarizer prompt.
fn write_session_with_prefix_compaction(
    path: &Path,
    summary_bytes: usize,
    pairs: usize,
    filler: usize,
) -> String {
    use std::io::Write as _;
    let summary = format!("PREFIX-COMPACTION-MARKER\n{}", "s".repeat(summary_bytes));
    let mut file = std::fs::File::create(path).expect("create session");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "11111111",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "C:\\\\repo",
        })
    )
    .expect("header");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "model_change", "id": "m0", "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "mock", "modelId": "mock-model",
        })
    )
    .expect("model change");
    let payload = "x".repeat(filler);
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "u000000", "parentId": "m0",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "user", "timestamp": 0,
                "content": [{"type": "text", "text": format!("before the checkpoint: {payload}")}],
            },
        })
    )
    .expect("user before compaction");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "a000000", "parentId": "u000000",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "assistant", "timestamp": 0,
                "api": "openai-completions", "provider": "mock", "model": "mock-model",
                "stopReason": "stop",
                "usage": {
                    "input": 1_500_000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 1_500_100,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                },
                "content": [{"type": "text", "text": format!("checkpoint answer: {payload}")}],
            },
        })
    )
    .expect("assistant before compaction");
    // The real on-disk shape (measured in ~/.pi sessions): the line starts
    // `{"type":"compaction",...}` and the summary alone is tens of KiB, so a
    // reader that stops at marker + 4 KiB truncates the JSON.
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "compaction",
            "id": "c0000000",
            "parentId": "a000000",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "summary": summary.clone(),
            "firstKeptEntryId": "c0000000",
            "tokensBefore": 1_500_000,
            "details": {"readFiles": [], "modifiedFiles": []},
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
            },
            "fromHook": false,
        })
    )
    .expect("compaction line");
    let mut parent = "c0000000".to_owned();
    for index in 1..=pairs {
        let user_id = format!("u{index:06}");
        let assistant_id = format!("a{index:06}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": user_id, "parentId": parent,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "user", "timestamp": 0,
                    "content": [{"type": "text", "text": format!("question {index}: {payload}")}],
                },
            })
        )
        .expect("user line");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": assistant_id, "parentId": user_id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "assistant", "timestamp": 0,
                    "api": "openai-completions", "provider": "mock", "model": "mock-model",
                    "stopReason": "stop",
                    "usage": {
                        "input": 1_500_000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 1_500_100,
                        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                    },
                    "content": [{"type": "text", "text": format!("answer {index}: {payload}")}],
                },
            })
        )
        .expect("assistant line");
        parent = assistant_id;
    }
    summary
}

/// Write a v3 JSONL session whose EARLY PREFIX holds a real-shape
/// `compaction` entry that RETAINS a message before itself
/// (`firstKeptEntryId` names an earlier message — the shape every compaction
/// appended by `append_compaction` has), followed by `pairs` ordinary
/// exchanges that push the whole group out of a small tail page.
///
/// Returns the retained tail's marker text so a test can prove it reached the
/// summarizer.
fn write_session_with_retained_prefix_compaction(
    path: &Path,
    summary_bytes: usize,
    pairs: usize,
    filler: usize,
) -> String {
    use std::io::Write as _;
    let summary = format!("PREFIX-COMPACTION-MARKER\n{}", "s".repeat(summary_bytes));
    let retained = format!("RETAINED-TAIL-MARKER\n{}", "r".repeat(filler));
    let mut file = std::fs::File::create(path).expect("create session");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "11111111",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "C:\\\\repo",
        })
    )
    .expect("header");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "model_change", "id": "m0", "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "mock", "modelId": "mock-model",
        })
    )
    .expect("model change");
    // History the checkpoint summarizes.
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "u000000", "parentId": "m0",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "user", "timestamp": 0,
                "content": [{"type": "text", "text": "SUMMARIZED-BEFORE-COMPACTION"}],
            },
        })
    )
    .expect("user before compaction");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "a000000", "parentId": "u000000",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "assistant", "timestamp": 0,
                "api": "openai-completions", "provider": "mock", "model": "mock-model",
                "stopReason": "stop",
                "usage": {
                    "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 2,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                },
                "content": [{"type": "text", "text": "pre-compaction answer"}],
            },
        })
    )
    .expect("assistant before compaction");
    // The RETAINED tail: the checkpoint's firstKeptEntryId points here, so
    // these messages are NOT covered by the checkpoint's summary.
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "u000001", "parentId": "a000000",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "user", "timestamp": 0,
                "content": [{"type": "text", "text": retained.clone()}],
            },
        })
    )
    .expect("retained user");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "a000001", "parentId": "u000001",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "assistant", "timestamp": 0,
                "api": "openai-completions", "provider": "mock", "model": "mock-model",
                "stopReason": "stop",
                "usage": {
                    "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 2,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                },
                "content": [{"type": "text", "text": "retained answer"}],
            },
        })
    )
    .expect("retained assistant");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "compaction",
            "id": "c0000000",
            "parentId": "a000001",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "summary": summary.clone(),
            "firstKeptEntryId": "u000001",
            "tokensBefore": 1_500_000,
            "details": {"readFiles": [], "modifiedFiles": []},
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
            },
            "fromHook": false,
        })
    )
    .expect("compaction line");
    let payload = "x".repeat(filler);
    let mut parent = "c0000000".to_owned();
    for index in 2..=pairs {
        let user_id = format!("u{index:06}");
        let assistant_id = format!("a{index:06}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": user_id, "parentId": parent,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "user", "timestamp": 0,
                    "content": [{"type": "text", "text": format!("question {index}: {payload}")}],
                },
            })
        )
        .expect("user line");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": assistant_id, "parentId": user_id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "assistant", "timestamp": 0,
                    "api": "openai-completions", "provider": "mock", "model": "mock-model",
                    "stopReason": "stop",
                    "usage": {
                        "input": 1_500_000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 1_500_100,
                        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                    },
                    "content": [{"type": "text", "text": format!("answer {index}: {payload}")}],
                },
            })
        )
        .expect("assistant line");
        parent = assistant_id;
    }
    retained
}

/// A resident span big enough to need several chunks under a small window.
fn long_span(messages: usize) -> Vec<Message> {
    let mut span = Vec::new();
    for index in 0..messages {
        span.push(user_msg(&format!("turn {index} {}", "u".repeat(400))));
        span.push(assistant_msg(
            &format!("reply {index} {}", "a".repeat(400)),
            100,
        ));
    }
    span
}

// ---------------------------------------------------------------------------
// Splitter units
// ---------------------------------------------------------------------------

#[test]
fn chunk_weights_are_the_exact_serialized_lengths() {
    let span = long_span(6);
    let budget = 900usize;
    let chunks = split_summarization_chunks(span.as_slice(), budget);
    assert!(chunks.len() > 1, "the fixture must split: {chunks:?}");
    for chunk in &chunks {
        let slice = &span[chunk.start..chunk.end];
        let serialized = serialize_conversation(slice).chars().count();
        assert_eq!(
            chunk.chars, serialized,
            "the recorded weight must equal the serialized text length"
        );
        if !chunk.oversize {
            assert!(
                chunk.chars <= budget,
                "a non-indivisible chunk must fit the cap: {chunk:?}"
            );
        }
    }
}

#[test]
fn chunk_cut_prefers_a_turn_boundary() {
    // VERDICT (test was wrong, implementation right): the rule is "when a turn
    // boundary exists INSIDE the accepted span, the chunk ends right before it"
    // (skipped when tool results were carried and for an indivisible message).
    // The first fixture let the greedy fit stop exactly AT the turn start, so
    // the boundary opened the next chunk instead — correct, but it did not
    // exercise the rule. This fixture puts the boundary inside the accepted
    // span: the cap stops after `[user, assistant, user]`, so the cut moves
    // back to the inner user message and the NEXT chunk opens the turn.
    // Weights: "[User]: " + text (8 + N), "[Assistant]: " + text (13 + N).
    let span = vec![
        user_msg(&"a".repeat(200)),         // 208
        assistant_msg(&"b".repeat(100), 1), // 113
        user_msg(&"c".repeat(60)),          // 68  <- the inner turn boundary
        assistant_msg(&"d".repeat(100), 1), // 113
        assistant_msg(&"e".repeat(100), 1), // 113
    ];
    let budget = 420usize;
    let chunks = split_summarization_chunks(span.as_slice(), budget);
    assert_eq!(chunks.len(), 2, "expected two chunks: {chunks:?}");
    assert_eq!(
        chunks[0].end, 2,
        "the first chunk must end right before the inner turn start: {chunks:?}"
    );
    assert_eq!(
        chunks[1].start, 2,
        "the second chunk must OPEN the inner turn: {chunks:?}"
    );
    assert!(matches!(span[chunks[1].start], Message::User(_)));
    // Every chunk still fits the cap.
    for chunk in &chunks {
        assert!(chunk.chars <= budget, "chunk over the cap: {chunk:?}");
    }
}

#[test]
fn chunk_cut_never_splits_a_fitting_tail() {
    // The counterpart of the test above, and the contract that keeps a SMALL
    // span at ONE summarizer call (upstream sends one): the turn-boundary
    // preference ends a chunk before a turn start so the NEXT chunk opens a
    // turn — there is no next chunk for the span's TAIL, so a tail that fits
    // the cap is never cut. Here the cap stops the first chunk mid-run and the
    // tail (which contains a turn start) stays whole.
    let span = vec![
        user_msg("turn one"),
        assistant_msg(&"a".repeat(300), 1),
        assistant_msg(&"b".repeat(300), 1),
        assistant_msg(&"c".repeat(300), 1),
        user_msg("turn two"),
        assistant_msg(&"d".repeat(300), 1),
    ];
    let budget = 700usize;
    let chunks = split_summarization_chunks(span.as_slice(), budget);
    assert_eq!(chunks.len(), 2, "expected two chunks: {chunks:?}");
    // Chunk 0 is where the cap stops (the accepted span holds no turn
    // boundary, so nothing to cut back to; message 4 would push it to 961
    // chars against the 700 cap)...
    assert_eq!(chunks[0].end, 3, "the first chunk ends where the cap stops");
    // ...and the tail is ONE chunk, its internal turn start (index 4) included.
    assert_eq!(chunks[1].start, 3);
    assert_eq!(chunks[1].end, span.len());
    assert!(
        (chunks[1].start..chunks[1].end).contains(&4),
        "the tail keeps its internal turn start: {chunks:?}"
    );
    for chunk in &chunks {
        assert!(chunk.chars <= budget, "chunk over the cap: {chunk:?}");
    }
}

#[test]
fn a_single_fitting_span_is_exactly_one_chunk() {
    // The regression this pins: a terse span whose last message OPENS A TURN
    // must not be split at that boundary — a first-turn tool call plus the
    // user's follow-up is one summarizer call (the branch-summarization
    // fixtures are exactly this shape).
    let span = vec![assistant_msg("called a tool", 1), user_msg("hello")];
    let chunks = split_summarization_chunks(span.as_slice(), 3_000);
    assert_eq!(chunks.len(), 1, "a fitting span is ONE chunk: {chunks:?}");
    assert_eq!(chunks[0].start, 0);
    assert_eq!(chunks[0].end, 2);
}

#[test]
fn chunk_never_opens_on_a_tool_result() {
    // A tool call whose result overflows the cap: the result must stay in the
    // SAME chunk as its call (a turn/tool pair is never split at its seam).
    let span = vec![
        user_msg("read the file"),
        assistant_with_tool_call("call-1"),
        tool_result("call-1", &"r".repeat(900)),
        user_msg("next turn"),
        assistant_msg("done", 1),
    ];
    let budget = 600usize;
    let chunks = split_summarization_chunks(span.as_slice(), budget);
    assert!(chunks.len() >= 2, "expected a split: {chunks:?}");
    for chunk in &chunks {
        assert!(
            !matches!(span[chunk.start], Message::ToolResult(_)),
            "a chunk must never open on a tool result: {chunks:?}"
        );
    }
    // The tool result rides with its call in the chunk that closed.
    let closing = &chunks[0];
    assert!(
        (closing.start..closing.end).contains(&2),
        "the tool result stays with its call: {chunks:?}"
    );
}

#[test]
fn an_oversize_single_message_stays_indivisible() {
    let huge = user_msg(&"h".repeat(5_000));
    let span = vec![user_msg("small"), huge.clone(), user_msg("after")];
    let budget = 1_000usize;
    let chunks = split_summarization_chunks(span.as_slice(), budget);
    let oversize = chunks
        .iter()
        .find(|chunk| chunk.oversize)
        .expect("the huge message must be marked indivisible");
    assert_eq!(oversize.len(), 1, "an oversize chunk holds one message");
    assert!(matches!(span[oversize.start], Message::User(_)));
    // The serialized chunk is truncated to the cap (never over it) by the
    // prompt builder; `serialize_chunk` is exercised through the chunked runs
    // below.
}

#[test]
fn chunk_budget_is_three_quarters_of_window_minus_reserve() {
    let model = test_model(128_000);
    let budget = summarization_chunk_budget(&model, 16_384);
    let expected_tokens = (128_000u64 - 16_384) * 3 / 4;
    assert_eq!(budget.tokens, expected_tokens);
    assert_eq!(budget.chars, (expected_tokens as f64 * 4.0) as usize);
    // A zero window falls back to the upstream 128k default, and an absurd
    // window degrades to the positive floor instead of a split loop.
    let zero = summarization_chunk_budget(&test_model(0), 16_384);
    assert_eq!(zero.tokens, expected_tokens);
    // A degenerate window (usable = 0) must still chunk and MUST NOT raise the
    // budget above the usable window: a 1 024-token floor let a chunk exceed a
    // 1 000-token window, which stopped the branch path from chunking a span
    // that cannot be sent in one request.
    let tiny = summarization_chunk_budget(&test_model(1_000), 16_384);
    assert!(
        tiny.tokens >= 1,
        "a degenerate window still chunks: {tiny:?}"
    );
    let small_window = summarization_chunk_budget(&test_model(1_000), 400);
    assert!(
        small_window.tokens <= 1_000 - 400,
        "the chunk budget never exceeds the usable window: {small_window:?}"
    );
}

// ---------------------------------------------------------------------------
// Streaming chunker == resident splitter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_chunker_produces_the_same_chunks_as_the_resident_splitter() {
    let span = long_span(8);
    // A small window so several chunks are required.
    let small = test_model(2_000);
    let resident_chunks = split_summarization_chunks(
        span.as_slice(),
        summarization_chunk_budget(&small, 128).chars,
    );
    assert!(resident_chunks.len() >= 2, "fixture must split");

    let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut chunker = StreamingChunker::new(
        &small,
        128,
        SpanFraming::History,
        None,
        recording_summarizer(Arc::clone(&prompts)),
    );
    for message in &span {
        chunker.push(message).await.expect("push");
    }
    let summarized = chunker.finish().await.expect("finish");
    assert_eq!(summarized.text, "SUMMARY");
    let prompts = prompts.lock().expect("prompt log").clone();
    assert_eq!(
        prompts.len(),
        resident_chunks.len(),
        "the streaming chunker must issue one call per resident chunk"
    );
    // Every streamed chunk is bounded by the same cap the resident splitter
    // computes, and the serialized conversation inside each prompt is the
    // resident chunk's serialization.
    for (index, chunk) in resident_chunks.iter().enumerate() {
        let expected = serialize_conversation(&span[chunk.range()]);
        assert!(
            prompts[index].contains(&expected),
            "chunk {index} must serialize exactly like the resident splitter"
        );
    }
}

// ---------------------------------------------------------------------------
// Chunked summarization stays inside the window budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chunked_summarization_bounds_every_request_by_the_window() {
    let span = long_span(40);
    let model = test_model(4_000);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 512,
        keep_recent_tokens: 100,
    };
    let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let usage = pir::compaction::summarize_messages_chunked_span(
        span.as_slice(),
        None,
        None,
        pir::compaction::SummarizationStyle::History,
        &model,
        &config,
        recording_summarizer(Arc::clone(&prompts)),
    )
    .await
    .expect("chunked summarization");
    assert!(usage.0 == "SUMMARY" || !usage.0.is_empty());
    let prompts = prompts.lock().expect("prompt log").clone();
    assert!(
        prompts.len() >= 2,
        "the fixture must need several calls: {}",
        prompts.len()
    );
    // The window budget in serialized characters: no request may exceed it.
    let window_chars = (model.context_window as f64 * 4.0) as usize;
    for (index, prompt) in prompts.iter().enumerate() {
        assert!(
            prompt.chars().count() <= window_chars,
            "request {index} exceeds the window budget: {} > {window_chars}",
            prompt.chars().count()
        );
    }
}

// ---------------------------------------------------------------------------
// Tail-first paging
// ---------------------------------------------------------------------------

#[test]
fn tail_page_parses_only_the_capped_window() {
    let dir = temp_dir("tail");
    let path = dir.join("big.jsonl");
    write_session(&path, 400, 1_000);
    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    assert!(file_bytes > 200 * 1024, "fixture must be large enough");

    let page = SessionTailPage::open(&path, 32 * 1024, 64)
        .expect("open page")
        .expect("a mappable file yields a page");
    let stats = page.stats();
    assert!(
        !stats.whole_file,
        "the fixture must be larger than the page"
    );
    assert!(
        stats.page_bytes <= 32 * 1024 + 64 * 1024,
        "the page stays inside the byte cap plus one scan block: {stats:?}"
    );
    assert!(
        stats.skipped_bytes >= file_bytes - stats.page_bytes - 1024,
        "the counters must account for the skipped prefix: {stats:?}"
    );
    assert!(
        stats.parsed_entries <= 64 + 1,
        "the entry cap binds too: {stats:?}"
    );
    // The page's last entry is the file's newest entry.
    let full = SessionManager::open(&path).expect("full open");
    let full_leaf = full.leaf_id().map(str::to_owned);
    assert_eq!(page.leaf_id().map(str::to_owned), full_leaf);

    // "Load more" walks back monotonically.
    let extended = page.extend(32 * 1024, 64).expect("extend").expect("page");
    assert!(extended.page_start() < page.page_start());
    assert_eq!(extended.window_end(), page.page_start());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn paged_open_keeps_the_window_and_widens_to_the_full_transcript() {
    let dir = temp_dir("paged-open");
    let path = dir.join("big.jsonl");
    write_session(&path, 200, 800);
    let full = SessionManager::open(&path).expect("full open");
    let full_entries = full.entries();

    let mut paged = SessionManager::open_paged(&path, 16 * 1024, 32).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the page caps");
    assert!(
        paged.resident_entries().len() < full_entries.len(),
        "only the tail window is resident"
    );
    let window = paged.paged_window().expect("paged window");
    assert!(window.page_start > 0);
    assert_eq!(
        paged.leaf_id().map(str::to_owned),
        full.leaf_id().map(str::to_owned),
        "the leaf comes from the tail window"
    );
    // VERDICT (implementation was wrong, test right): the first paged open
    // loaded a header-less entry list straight from the page, so `load_entries`
    // fell back to `new_session` and invented a fresh session id — opening a
    // huge session mis-identified it. `open_paged` now prepends the file's real
    // header (read from the first line by a bounded probe), so the id, the cwd,
    // the header entry and the print host's JSON header line are the file's.
    assert_eq!(paged.id(), full.id(), "the session id is the file's");
    assert_eq!(paged.cwd(), full.cwd(), "the cwd comes from the header");
    assert_eq!(
        paged.header().map(|header| header.id.clone()),
        full.header().map(|header| header.id.clone()),
        "the paged manager reports the FILE's header"
    );
    assert_eq!(paged.header().and_then(|header| header.version), Some(3));

    // Widening restores byte-identical entries and clears the paged flag.
    paged.widen_to_full().expect("widen");
    assert!(!paged.is_paged());
    assert_eq!(paged.entries(), full_entries);

    // VERDICT (test was wrong, implementation right): a RENAME appends a
    // `session_info` entry (upstream `appendSessionInfo`), which paged mode
    // supports — the append path writes only the newest entry, so the skipped
    // prefix is never touched. What a paged manager refuses is a REWRITE
    // (migration / branch-to-new-file); that refusal is pinned separately.
    let mut paged_again = SessionManager::open_paged(&path, 16 * 1024, 32).expect("paged open");
    let before = std::fs::read(&path).expect("read before rename");
    paged_again
        .set_session_name(Some("renamed"))
        .expect("a rename is an append, which paged mode supports");
    let after = std::fs::read(&path).expect("read after rename");
    assert!(after.len() > before.len(), "the rename appended one line");
    assert_eq!(
        &after[..before.len()],
        before.as_slice(),
        "the skipped prefix stays byte-identical"
    );
    assert_eq!(paged_again.session_name().as_deref(), Some("renamed"));
    std::fs::remove_dir_all(&dir).ok();
}

/// A paged open REFUSES a migration that would rewrite the file (the rewrite
/// would drop the unread prefix) instead of silently writing the tail window
/// back as if it were the whole session. The refusal is the paged guard, not a
/// parse failure: the full open migrates the same file.
#[test]
fn paged_open_refuses_a_migration_rewrite_and_leaves_the_file_alone() {
    let dir = temp_dir("paged-migrate");
    let path = dir.join("legacy.jsonl");
    write_session(&path, 40, 300);
    // Rewrite the header as a v2 session: opening it triggers the v2→v3
    // migration, which rewrites the whole file.
    let contents = std::fs::read_to_string(&path).expect("read fixture");
    let legacy = contents.replacen("\"version\":3", "\"version\":2", 1);
    assert_ne!(legacy, contents, "the fixture must carry a version field");
    std::fs::write(&path, &legacy).expect("write v2 fixture");
    let before = std::fs::read(&path).expect("read before");

    let error = SessionManager::open_paged(&path, 8 * 1024, 16)
        .expect_err("a paged open must refuse the migration rewrite");
    assert!(
        error.to_string().contains("tail page"),
        "the refusal names the paged mode: {error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read after"),
        before,
        "a refused open must not modify the file"
    );

    // The full open migrates the same file (the guard is paged-specific).
    let migrated = SessionManager::open(&path).expect("full open migrates");
    assert_eq!(migrated.header().and_then(|header| header.version), Some(3));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn appends_through_a_paged_open_preserve_the_prefix() {
    let dir = temp_dir("paged-append");
    let path = dir.join("big.jsonl");
    write_session(&path, 200, 800);
    let before = std::fs::read(&path).expect("read before");
    let mut paged = SessionManager::open_paged(&path, 16 * 1024, 32).expect("paged open");
    assert!(paged.is_paged());
    paged
        .append_message(user_msg("a new prompt after a paged open"))
        .expect("append");
    let after = std::fs::read(&path).expect("read after");
    assert!(after.len() > before.len(), "the append must grow the file");
    assert_eq!(
        &after[..before.len()],
        before.as_slice(),
        "the prefix must be byte-identical (append-only, never rewritten)"
    );
    // The resident list is header-first, like a full open.
    assert!(matches!(
        paged.resident_entries().first().map(|entry| entry.as_ref()),
        Some(FileEntry::Session(_))
    ));
    // The appended entry is resident and is the leaf.
    assert!(paged
        .resident_entries()
        .iter()
        .any(|entry| matches!(entry.as_ref(), FileEntry::Message(inner)
            if inner.id == paged.leaf_id().unwrap_or_default())));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn paged_compaction_streams_the_prefix_with_bounded_requests() {
    let dir = temp_dir("paged-compact");
    let path = dir.join("big.jsonl");
    write_session(&path, 400, 900);
    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    assert!(file_bytes > 300 * 1024, "fixture must be large");

    let session = SessionManager::open_paged(&path, 32 * 1024, 64).expect("paged open");
    assert!(session.is_paged());
    let model = test_model(4_000);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 512,
        keep_recent_tokens: 100,
    };
    let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let paged = compact_paged(
        &session,
        &model,
        &config,
        recording_summarizer(Arc::clone(&prompts)),
        None,
    )
    .await
    .expect("paged compaction");
    let stats = paged.stats;
    assert!(
        stats.prefix_bytes > 0,
        "the prefix must be streamed from disk: {stats:?}"
    );
    assert!(
        stats.prefix_entries > 0,
        "the prefix's entries contributed to the span: {stats:?}"
    );
    let prompts = prompts.lock().expect("prompt log").clone();
    assert!(prompts.len() >= 2, "the span must chunk: {}", prompts.len());
    let window_chars = (model.context_window as f64 * 4.0) as usize;
    for (index, prompt) in prompts.iter().enumerate() {
        assert!(
            prompt.chars().count() <= window_chars,
            "request {index} exceeds the window budget: {} > {window_chars}",
            prompt.chars().count()
        );
    }
    // The result keeps a bounded tail and a real cut point.
    assert!(
        paged.result.retained_tail.len() < 32,
        "the retained tail is the keep-recent window"
    );
    assert!(paged.result.first_kept_entry_id.is_some());
    assert!(paged.result.summary.contains("SUMMARY"));
    // The resident window did NOT grow to the whole file.
    assert!(session.resident_entries().len() < 400);
    std::fs::remove_dir_all(&dir).ok();
}

/// The paged (disk-backed) path must carry `/compact <instructions>` into
/// EVERY summarizer call — the streamed chunk calls AND the split-turn prefix
/// call — instead of discarding it (`let _ = custom_instructions;` was the
/// bug). With no instructions the prompts carry no focus text at all.
#[tokio::test]
async fn paged_compaction_forwards_custom_instructions_to_every_summarizer_call() {
    let dir = temp_dir("paged-focus");
    let path = dir.join("big.jsonl");
    write_session(&path, 400, 900);
    let session = SessionManager::open_paged(&path, 32 * 1024, 64).expect("paged open");
    assert!(session.is_paged(), "the fixture must be paged");
    let model = test_model(4_000);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 512,
        keep_recent_tokens: 100,
    };

    let focus = "focus on the payment flow";
    let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    compact_paged(
        &session,
        &model,
        &config,
        recording_summarizer(Arc::clone(&prompts)),
        Some(focus),
    )
    .await
    .expect("paged compaction with focus");
    let focused = prompts.lock().expect("prompt log").clone();
    assert!(focused.len() >= 2, "the span must chunk: {}", focused.len());
    let suffix = format!("\n\nAdditional focus: {focus}");
    for (index, prompt) in focused.iter().enumerate() {
        let tail_start = prompt.chars().count().saturating_sub(120);
        let tail: String = prompt.chars().skip(tail_start).collect();
        assert!(
            prompt.ends_with(&suffix),
            "paged prompt {index} must carry the instructions at its tail: {tail}"
        );
    }
    assert!(
        focused
            .iter()
            .any(|prompt| prompt.starts_with("# Conversation\n")),
        "the fixture splits a turn, so the turn-prefix prompt must carry the focus too: {:?}",
        focused
            .iter()
            .map(|prompt| prompt.chars().take(20).collect::<String>())
            .collect::<Vec<_>>()
    );

    // The SAME fixture without instructions: no focus text is invented (the
    // streaming chunker's default is `None`).
    let plain_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    compact_paged(
        &session,
        &model,
        &config,
        recording_summarizer(Arc::clone(&plain_prompts)),
        None,
    )
    .await
    .expect("paged compaction without focus");
    let plain = plain_prompts.lock().expect("prompt log").clone();
    assert!(!plain.is_empty());
    for (index, prompt) in plain.iter().enumerate() {
        assert!(
            !prompt.contains("Additional focus:"),
            "paged prompt {index} must be unchanged without instructions"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The turn-prefix prompt is byte-identical to the pre-focus shape when no
/// instructions are given, and appends upstream's `Additional focus:` suffix
/// when they are (compaction.ts:1078-1081).
#[tokio::test]
async fn turn_prefix_prompt_without_instructions_is_byte_identical_and_focus_appends() {
    let span = vec![user_msg("u1"), assistant_msg("a1", 1)];
    let model = test_model(128_000);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 512,
        keep_recent_tokens: 100,
    };
    let expected = format!(
        "# Conversation\n{}\n\n# Instructions\n{}",
        serialize_conversation(&span),
        TURN_PREFIX_SUMMARIZATION_PROMPT
    );

    let without: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    summarize_messages_chunked_span(
        span.as_slice(),
        None,
        None,
        SummarizationStyle::TurnPrefix,
        &model,
        &config,
        recording_summarizer(Arc::clone(&without)),
    )
    .await
    .expect("turn prefix summary");
    let prompts = without.lock().expect("prompt log").clone();
    assert_eq!(prompts.len(), 1, "a fitting span is one call");
    assert_eq!(
        prompts[0], expected,
        "no instructions must leave the prompt byte-identical"
    );

    let with: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    summarize_messages_chunked_span(
        span.as_slice(),
        None,
        Some("focus on retries"),
        SummarizationStyle::TurnPrefix,
        &model,
        &config,
        recording_summarizer(Arc::clone(&with)),
    )
    .await
    .expect("turn prefix summary with focus");
    let prompts = with.lock().expect("prompt log").clone();
    assert_eq!(prompts.len(), 1, "a fitting span is one call");
    assert_eq!(
        prompts[0],
        format!("{expected}\n\nAdditional focus: focus on retries")
    );
}

/// A compaction line in the SKIPPED PREFIX must be READABLE: real entries are
/// tens of KiB (the marker sits at byte ~1 and the JSON's newline is far
/// outside a marker+4 KiB window), so the old fixed window truncated the JSON
/// and `compact_paged` failed every threshold turn instead of resuming.
///
/// This fixture puts a real-shape compaction entry (a ~20 KiB summary) older
/// than the tail page caps, so `scan_prefix` must read that line from disk;
/// the run must resume from it: the stats flag, and the first summarizer
/// prompt carrying `<previous-summary>` with the fixture's summary text.
#[tokio::test]
async fn paged_compaction_resumes_from_a_real_shaped_prefix_compaction_line() {
    let dir = temp_dir("paged-resume-prefix");
    let path = dir.join("big.jsonl");
    let summary = write_session_with_prefix_compaction(&path, 20 * 1024, 400, 900);
    assert!(
        summary.len() > 4 * 1024,
        "the fixture's compaction line must exceed the old marker+4 KiB window"
    );

    let session = SessionManager::open_paged(&path, 32 * 1024, 64).expect("paged open");
    assert!(session.is_paged(), "the fixture must be paged");
    assert!(
        !session
            .resident_entries()
            .iter()
            .any(|entry| matches!(entry.as_ref(), FileEntry::Compaction(_))),
        "the compaction entry must live in the SKIPPED prefix"
    );

    let model = test_model(4_000);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 512,
        keep_recent_tokens: 100,
    };
    let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let paged = compact_paged(
        &session,
        &model,
        &config,
        recording_summarizer(Arc::clone(&prompts)),
        None,
    )
    .await
    .expect("a real-shaped prefix compaction line must be readable");
    assert!(
        paged.stats.resumed_from_prefix_compaction,
        "the span must start after the prefix compaction: {:?}",
        paged.stats
    );
    let prompts = prompts.lock().expect("prompt log").clone();
    assert!(
        !prompts.is_empty(),
        "the span after the compaction must reach the summarizer"
    );
    assert!(
        prompts[0].contains("<previous-summary>"),
        "the resumed checkpoint must be in the first prompt: {}",
        prompts[0].chars().take(400).collect::<String>()
    );
    assert!(
        prompts[0].contains("PREFIX-COMPACTION-MARKER"),
        "the prefix compaction's summary text must reach the summarizer prompt"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A prefix compaction that RETAINS a tail before itself (`firstKeptEntryId`
/// names an earlier entry — the shape every `append_compaction` writes) must
/// be resumed from that first kept entry, not from the compaction line.
///
/// The checkpoint summary only covers the messages BEFORE `firstKeptEntryId`;
/// the messages between it and the checkpoint are still part of the model
/// context. Starting the streamed span at the compaction line dropped them
/// from the new summary and from the rebuilt context: a silent history loss
/// on the paged (large-session) compaction path.
#[tokio::test]
async fn paged_compaction_summarizes_the_retained_tail_of_a_prefix_compaction() {
    let dir = temp_dir("paged-retained-tail");
    let path = dir.join("big.jsonl");
    let retained = write_session_with_retained_prefix_compaction(&path, 20 * 1024, 400, 900);
    assert!(retained.contains("RETAINED-TAIL-MARKER"));

    let session = SessionManager::open_paged(&path, 32 * 1024, 64).expect("paged open");
    assert!(session.is_paged(), "the fixture must be paged");
    assert!(
        !session
            .resident_entries()
            .iter()
            .any(|entry| matches!(entry.as_ref(), FileEntry::Compaction(_))),
        "the compaction entry must live in the SKIPPED prefix"
    );
    assert!(
        !session.resident_entries().iter().any(|entry| matches!(
            entry.as_ref(),
            FileEntry::Message(inner) if inner.id == "u000001"
        )),
        "the retained tail must live in the skipped prefix"
    );

    let model = test_model(4_000);
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 512,
        keep_recent_tokens: 100,
    };
    let prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let paged = compact_paged(
        &session,
        &model,
        &config,
        recording_summarizer(Arc::clone(&prompts)),
        None,
    )
    .await
    .expect("paged compaction");
    let prompts = prompts.lock().expect("prompt log").clone();
    assert!(!prompts.is_empty(), "the span must reach the summarizer");
    let all = prompts.join("\n");
    assert!(
        all.contains("RETAINED-TAIL-MARKER"),
        "the checkpoint's retained tail must be summarized, not dropped: {:?}",
        prompts
            .iter()
            .map(|prompt| prompt.chars().take(200).collect::<String>())
            .collect::<Vec<_>>()
    );
    assert!(paged.result.summary.contains("SUMMARY"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn full_save_fidelity_and_fork_are_untouched_by_paging() {
    let dir = temp_dir("paged-fork");
    let path = dir.join("big.jsonl");
    write_session(&path, 60, 400);
    let mut paged = SessionManager::open_paged(&path, 8 * 1024, 16).expect("paged open");
    paged.widen_to_full().expect("widen");
    let source_entries = paged.entries();
    let fork = SessionManager::fork_from_source(&path, "C:\\\\repo", &dir, Some("22222222"))
        .expect("fork");
    let forked_entries = fork.entries();
    assert_eq!(
        forked_entries.len(),
        source_entries.len(),
        "fork carries every entry verbatim"
    );
    // VERDICT (test was wrong, implementation right): `fork_from_source` sets
    // `parentSession` to the source FILE PATH, matching upstream `forkFrom`
    // (`header.parent_session = Some(source_path.to_string_lossy())`), not the
    // source session id.
    assert_eq!(
        fork.parent_session(),
        Some(path.to_string_lossy().as_ref()),
        "a fork records its source path as the parent session"
    );
    assert_eq!(fork.id(), "22222222", "the fork uses the requested id");
    assert_eq!(fork.entries(), source_entries);
    std::fs::remove_dir_all(&dir).ok();
}

/// The "load more" walk (the TUI's scroll-up seam): every
/// `read_older_page` call moves strictly older, never scans the whole prefix,
/// terminates at the file start with `None`, and the concatenated projection
/// form equals a FULL load's projection for the same range.
#[test]
fn paged_prefix_cursor_walks_older_pages_until_the_file_start() {
    let dir = temp_dir("paged-cursor");
    let path = dir.join("big.jsonl");
    write_session(&path, 300, 700);
    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    let paged = SessionManager::open_paged(&path, 8 * 1024, 16).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the caps");
    let mut cursor = paged.prefix_cursor().expect("a paged session has a cursor");
    assert!(cursor > 0 && cursor < file_bytes);

    // The newest band first (the resident window), then older pages prepended.
    let mut collected: Vec<Message> = paged
        .resident_entries()
        .iter()
        .filter(|entry| !entry.is_header())
        .flat_map(|entry| {
            pir::session_manager::projection::session_entry_to_context_messages(entry)
        })
        .collect();
    let mut pages = 0usize;
    while let Some(page) = paged
        .read_older_page(cursor, 8 * 1024, 16)
        .expect("older page")
    {
        pages += 1;
        let stats = page.stats();
        assert!(
            page.page_start() < cursor,
            "every call must move strictly older: {} !< {cursor}",
            page.page_start()
        );
        assert!(
            stats.page_bytes <= 8 * 1024 + 4 * 1024,
            "an older page stays inside the byte cap: {stats:?}"
        );
        assert!(
            stats.parsed_entries <= 17,
            "an older page stays inside the entry cap: {stats:?}"
        );
        assert!(
            !stats.whole_file && stats.page_bytes < file_bytes,
            "an older page must never cover (or scan) the whole file: {stats:?}"
        );
        let older: Vec<Message> = page
            .entries()
            .iter()
            .flat_map(|entry| {
                pir::session_manager::projection::session_entry_to_context_messages(entry)
            })
            .collect();
        let mut merged = older;
        merged.extend(collected);
        collected = merged;
        cursor = page.page_start();
        assert!(pages < 400, "the cursor walk must terminate");
    }
    assert!(pages >= 2, "the fixture must need several pages: {pages}");
    assert_eq!(cursor, 0, "the walk terminates at the file start");
    assert!(
        paged
            .read_older_page(0, 8 * 1024, 16)
            .expect("start probe")
            .is_none(),
        "the file start has no older page"
    );
    // A non-paged session has no cursor and no older pages.
    let full = SessionManager::open(&path).expect("full open");
    assert!(full.prefix_cursor().is_none());
    assert!(full
        .read_older_page(file_bytes, 8 * 1024, 16)
        .expect("non-paged probe")
        .is_none());

    // The projection form matches a full load for the same range.
    let expected = full.build_context_entries().expect("full projection");
    assert_eq!(
        collected, expected,
        "the paged walk's projection must equal the full load's"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn projection_meta_and_the_projection_agree_on_message_counts() {
    // The compaction basis reads per-entry COUNTS (no message copy); the
    // counts must equal what the projection materializes, or the cut point
    // would drift.
    let mut session = SessionManager::in_memory("C:/repo", None).expect("in-memory session");
    session.append_message(user_msg("q1")).expect("append");
    session
        .append_message(assistant_msg("a1", 10))
        .expect("append");
    session.append_message(user_msg("q2")).expect("append");
    session
        .append_compaction("an earlier summary", Some("u000000"), 12, None)
        .expect("compaction");
    session
        .append_message(assistant_msg("after the checkpoint", 20))
        .expect("append");
    let projection: SessionProjection = session.build_session_projection().expect("projection");
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    assert_eq!(metas.len(), projection.entries.len());
    let total: usize = metas.iter().map(|meta| meta.message_count).sum();
    assert_eq!(
        total,
        projection.messages.len(),
        "the metadata counts must sum to the projection's messages"
    );
    // And the borrowed refs slice the live list exactly like the projection's
    // own ranges do.
    let refs = ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
    for (index, entry) in projection.entries.iter().enumerate() {
        assert_eq!(
            refs[index].messages,
            entry.messages_in(&projection.messages),
            "entry {index} must slice identically"
        );
    }
    // The borrowed preparation plans the same cut as the owned one.
    let borrowed =
        prepare_compaction_refs(&refs, &[] as &[FileEntry], &CompactionConfig::default());
    let owned = {
        let cut_entries: Vec<pir::compaction::CutEntry> = projection
            .entries
            .iter()
            .map(|entry| pir::compaction::CutEntry::from_projection(entry, &projection.messages))
            .collect();
        pir::compaction::prepare_compaction(
            &cut_entries,
            &[] as &[FileEntry],
            &CompactionConfig::default(),
        )
    };
    match (borrowed, owned) {
        (Some(borrowed), Some(owned)) => {
            assert_eq!(borrowed.first_kept_entry_id, owned.first_kept_entry_id);
            assert_eq!(
                borrowed.messages_to_summarize.len(),
                owned.messages_to_summarize.len()
            );
            assert_eq!(borrowed.kept_messages.len(), owned.kept_messages.len());
            assert_eq!(borrowed.is_split_turn, owned.is_split_turn);
        }
        (None, None) => {}
        other => panic!("the borrowed and owned preparations must agree: {other:?}"),
    }
}

#[test]
fn cut_refs_rebases_a_rebuilt_context_that_dropped_the_checkpoint_system_replay() {
    // A checkpoint stores the current system message; the session projection
    // replays it as its first message. The in-memory context a successful
    // compaction returns is only `[summary, retained tail]` — it does NOT
    // replay that system message. Feeding the projection's metadata to
    // `cut_refs` with that rebuilt context must not shift the entry ranges:
    // the retained message has to stay its own entry's message instead of
    // being swallowed by the checkpoint's leading range.
    let mut session = SessionManager::in_memory("C:/repo", None).expect("in-memory session");
    session.append_message(user_msg("q1")).expect("append");
    session
        .append_message(assistant_msg("a1", 10))
        .expect("append");
    session
        .append_message(system_msg("system prompt replay"))
        .expect("append");
    session
        .append_compaction("checkpoint summary", None, 100, None)
        .expect("compaction");
    let kept_id = session.append_message(user_msg("q2")).expect("append");

    let projection: SessionProjection = session.build_session_projection().expect("projection");
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    assert!(
        matches!(projection.messages.first(), Some(Message::System(_))),
        "the checkpoint must replay its stored system message: {:?}",
        projection.messages
    );
    assert_eq!(metas[0].message_count, 2, "system replay + summary");
    assert_eq!(projection.messages.len(), 3, "system replay, summary, q2");

    // The context a compaction returns: summary + retained tail (no replay).
    let rebuilt = &projection.messages[1..];
    let refs = ProjectionEntryMeta::cut_refs(&metas, rebuilt);
    assert_eq!(refs.len(), metas.len());
    assert_eq!(
        refs[0].messages.len(),
        1,
        "the checkpoint contributes only its summary to the rebuilt context"
    );
    assert!(matches!(refs[0].messages[0], Message::User(_)));
    assert_eq!(
        refs[1].id,
        Some(kept_id.as_str()),
        "the retained message keeps its own entry"
    );
    assert_eq!(refs[1].messages.len(), 1);
    assert_eq!(
        refs[1].messages[0], projection.messages[2],
        "q2 must not be swallowed by the checkpoint range"
    );
    let covered: usize = refs.iter().map(|reference| reference.messages.len()).sum();
    assert_eq!(
        covered,
        rebuilt.len(),
        "every live message is covered exactly once"
    );
}

#[test]
fn default_page_caps_are_bounded() {
    const { assert!(DEFAULT_TAIL_PAGE_BYTES <= 8 * 1024 * 1024) };
    const { assert!(DEFAULT_TAIL_PAGE_ENTRIES <= 8192) };
}
