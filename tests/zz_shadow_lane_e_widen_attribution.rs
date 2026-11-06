//! Lane-E attribution harness (ignored by default): where the print-widen
//! peak comes from.
//!
//! Run explicitly:
//! ```text
//! cargo test --test lane_e_widen_attribution -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The fixture is a 100 MB linear v3 session (the same shape
//! `scripts/perf/big_session_probe.py` synthesizes). Phase 1 walks the print
//! fast path (`build_paged_prompt_context` -> `OpenAiStreamedBody` serialize);
//! phase 2 walks the widen fallback (`widen_to_full` -> projection) on a fresh
//! manager. DHAT's live/peak counters are printed per phase so each resident
//! transcript copy is visible.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pir::ai::api::openai::{build_openai_body, build_openai_body_streamed, OpenAiStreamedBody};
use pir::ai::types::{
    ContentBlock, Context, Model, ModelCost, ModelCostRates, StreamOptions, TextContent,
    UserMessage, UserRole,
};
use pir::session_manager::{SessionManager, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES};

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-lane-e-widen-{label}-{}-{stamp}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn write_fixture(path: &Path, target_bytes: u64) {
    let payload = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(20);
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create"));
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session", "version": 3, "id": "lane-e-fixture",
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "C:\\\\repo",
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "model_change", "id": "m0", "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "provider": "mock", "modelId": "mock-model",
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "thinking_level_change", "id": "t0", "parentId": "m0",
            "timestamp": "2026-01-01T00:00:00.000Z", "thinkingLevel": "off",
        })
    )
    .unwrap();
    let stamp = "2026-01-01T00:00:00.000Z";
    let mut written = 0u64;
    let mut index = 0usize;
    let mut parent = "t0".to_owned();
    while written < target_bytes {
        index += 1;
        let user_id = format!("u{index:06}");
        let asst_id = format!("a{index:06}");
        let usage = serde_json::json!({
            "input": 1_500_000u64 + (index as u64) * 250u64,
            "output": 120, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 1_500_120u64 + (index as u64) * 250u64,
            "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0},
        });
        let user = serde_json::json!({
            "type": "message", "id": user_id, "parentId": parent, "timestamp": stamp,
            "message": {"role": "user", "timestamp": 0,
                "content": [{"type": "text", "text": format!("question {index}: {payload}")}]},
        });
        let asst = serde_json::json!({
            "type": "message", "id": asst_id, "parentId": user_id, "timestamp": stamp,
            "message": {"role": "assistant", "timestamp": 0,
                "api": "openai-completions", "provider": "mock", "model": "mock-model",
                "stopReason": "stop", "usage": usage,
                "content": [{"type": "text", "text": format!("answer {index}: {payload}")}]},
        });
        for entry in [user, asst] {
            let line = entry.to_string();
            file.write_all(line.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
            written += line.len() as u64 + 1;
        }
        parent = asst_id;
    }
    file.flush().unwrap();
}

fn mock_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock model".into(),
        api: "openai-completions".into(),
        provider: "mock".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
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
        context_window: 100_000_000,
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

fn user_message(text: &str) -> pir::ai::types::Message {
    pir::ai::types::Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 0,
    })
}

fn report(label: &str, stats: &dhat::HeapStats, base: &dhat::HeapStats) {
    println!(
        "[dhat] {label}: live={} (+{}) peak={} (+{}) blocks={}",
        stats.curr_bytes,
        stats.curr_bytes as i128 - base.curr_bytes as i128,
        stats.max_bytes,
        stats.max_bytes as i128 - base.curr_bytes as i128,
        stats.curr_blocks,
    );
}

#[test]
#[ignore = "lane-E attribution harness; run explicitly with --nocapture"]
fn lane_e_widen_attribution() {
    let dir = temp_dir("attr");
    let path = dir.join("big.jsonl");
    write_fixture(&path, 100 * 1024 * 1024);
    let file_bytes = std::fs::metadata(&path).unwrap().len();
    println!("[dhat] fixture: {file_bytes} bytes");

    let profile_path = dir.join("profile.json");
    let profiler = dhat::Profiler::builder().file_name(&profile_path).build();
    let base = dhat::HeapStats::get();
    println!(
        "[dhat] base: live={} blocks={}",
        base.curr_bytes, base.curr_blocks
    );

    // ---- Phase 1: print fast path (paged context -> streamed request) ----
    let mut manager =
        SessionManager::open_paged(&path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
            .expect("paged open");
    assert!(manager.is_paged());
    let after_open = dhat::HeapStats::get();
    report("after paged open", &after_open, &base);

    manager
        .append_message(user_message("Reply with OK"))
        .expect("append user");
    let after_append = dhat::HeapStats::get();
    report("after append user", &after_append, &base);

    let paged = manager
        .build_paged_prompt_context()
        .expect("paged context")
        .expect("linear session");
    let after_ctx = dhat::HeapStats::get();
    report("after paged Vec<Message>", &after_ctx, &base);
    println!("[dhat] paged messages: {}", paged.messages.len());
    let mut text_bytes = 0usize;
    for message in &paged.messages {
        let blocks = match message {
            pir::ai::types::Message::User(m)
            | pir::ai::types::Message::System(m)
            | pir::ai::types::Message::Developer(m) => &m.content,
            pir::ai::types::Message::Assistant(m) => &m.content,
            pir::ai::types::Message::ToolResult(m) => &m.content,
        };
        for block in blocks {
            if let ContentBlock::Text(text) = block {
                text_bytes += text.text.len();
            }
        }
    }
    println!(
        "[dhat] text bytes={text_bytes} size_of Message={} ContentBlock={} UserMessage={} AssistantMessage={} ToolResultMessage={}",
        std::mem::size_of::<pir::ai::types::Message>(),
        std::mem::size_of::<ContentBlock>(),
        std::mem::size_of::<pir::ai::types::UserMessage>(),
        std::mem::size_of::<pir::ai::types::AssistantMessage>(),
        std::mem::size_of::<pir::ai::types::ToolResultMessage>(),
    );

    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_owned()),
        messages: paged.messages,
        tools: None,
    };
    let model = mock_model();
    let options = StreamOptions::default();
    let compat = pir::ai::types::OpenAiCompat::default();
    let body: OpenAiStreamedBody<'_> =
        build_openai_body_streamed(&model, &context, &options, &compat).expect("streamed body");
    let estimate = body.estimated_json_bytes();
    println!("[dhat] estimated body bytes: {estimate}");
    let mut payload: Vec<u8> = Vec::with_capacity(estimate);
    serde_json::to_writer(&mut payload, &body).expect("serialize");
    let after_serialize = dhat::HeapStats::get();
    report("after serialized request body", &after_serialize, &base);
    println!("[dhat] payload bytes: {}", payload.len());
    drop(payload);
    let value_body = build_openai_body(&model, &context, &options, &compat).expect("value body");
    let after_value_body = dhat::HeapStats::get();
    report(
        "after value body (before_provider_request path)",
        &after_value_body,
        &base,
    );
    drop(value_body);
    drop(body);
    drop(context);
    drop(manager);
    let after_drop = dhat::HeapStats::get();
    report("after drop phase 1", &after_drop, &base);

    // ---- Phase 2: widen fallback (entries + projection) ----
    let mut manager =
        SessionManager::open_paged(&path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
            .expect("paged open 2");
    manager
        .append_message(user_message("Reply with OK"))
        .expect("append user 2");
    manager.widen_to_full().expect("widen");
    let after_widen = dhat::HeapStats::get();
    report("after widen_to_full", &after_widen, &base);
    let projection = manager.build_session_projection().expect("projection");
    let after_projection = dhat::HeapStats::get();
    report(
        "after widen projection Vec<Message>",
        &after_projection,
        &base,
    );
    println!("[dhat] projection messages: {}", projection.messages.len());
    drop(projection.messages);
    drop(manager);

    let final_stats = dhat::HeapStats::get();
    report("final", &final_stats, &base);
    drop(profiler);
    println!("[dhat] profile: {}", profile_path.display());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Message slot sizes
// ---------------------------------------------------------------------------

// Boxing `deferred` and `nested_calls` keeps the per-message slot small:
// `AssistantMessage` 592 -> 400 B, `ToolResultMessage` 424 -> 400 B, and
// `Message` 592 -> 408 B on 64-bit. Pin the measured sizes so a field change
// cannot silently re-inflate every message slot.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<pir::ai::types::Message>() == 408);
    assert!(std::mem::size_of::<pir::ai::types::AssistantMessage>() == 400);
    assert!(std::mem::size_of::<pir::ai::types::ToolResultMessage>() == 400);
};

#[test]
fn message_slot_sizes_are_pinned() {
    let message = std::mem::size_of::<pir::ai::types::Message>();
    let assistant = std::mem::size_of::<pir::ai::types::AssistantMessage>();
    let tool_result = std::mem::size_of::<pir::ai::types::ToolResultMessage>();
    println!("size_of(Message) = {message}");
    println!("size_of(AssistantMessage) = {assistant}");
    println!("size_of(ToolResultMessage) = {tool_result}");
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(message, 408);
        assert_eq!(assistant, 400);
        assert_eq!(tool_result, 400);
    }
}
