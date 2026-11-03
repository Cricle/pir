//! Resident-compaction peak attribution harness (ignored by default).
//!
//! Mirrors the print host's 100 MB resident compaction path and prints DHAT
//! live/peak bytes per phase:
//!
//! 1. tail-first open (the print host's `open_session_tail_first`);
//! 2. `widen_to_full` (the resident entry list);
//! 3. `build_session_projection` (the prompt context aggregate);
//! 4. the compaction cut basis (`ProjectionEntryMeta` + `CutEntryRef`);
//! 5. `prepare_compaction_refs` (borrowed span);
//! 6. the chunked summarizer, with every request serialized exactly like the
//!    OpenAI streamed body the provider would send;
//! 7. `rebuilt_context`;
//! 8. `append_compaction_with_usage`.
//!
//! Run explicitly:
//! ```text
//! cargo test --test compaction_peak_attribution --locked -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The fixture is byte-shape-identical to
//! `logs/lane-speed-rss/resident-100mb-contextedit.jsonl` (linear v3 session
//! plus one trailing `context_edit` that forces the resident pipeline).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::ai::api::openai::{build_openai_body_streamed, OpenAiStreamedBody};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    OpenAiCompat, StopReason, StreamOptions, TextContent, Usage,
};
use pir::compaction::{self, CompactionConfig, CompleteFn, ProjectionEntryMeta};
use pir::session_manager::{SessionManager, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-compaction-peak-{label}-{}-{stamp}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

/// Same byte shape as `lane_e_widen_attribution::write_fixture`, plus the
/// trailing `context_edit` the resident-compaction fixture carries.
fn write_fixture(path: &Path, target_bytes: u64) {
    let payload = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(20);
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create"));
    let stamp = "2026-01-01T00:00:00.000Z";
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session", "version": 3, "id": "compaction-peak-fixture",
            "timestamp": stamp, "cwd": "C:\\\\repo",
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "model_change", "id": "m0", "parentId": null,
            "timestamp": stamp, "provider": "mock", "modelId": "mock-model",
        })
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "thinking_level_change", "id": "t0", "parentId": "m0",
            "timestamp": stamp, "thinkingLevel": "off",
        })
    )
    .unwrap();
    let mut written = 0u64;
    let mut index = 0usize;
    let mut parent = "t0".to_owned();
    let mut first_user = String::new();
    while written < target_bytes {
        index += 1;
        let user_id = format!("u{index:06}");
        let asst_id = format!("a{index:06}");
        if index == 1 {
            first_user = user_id.clone();
        }
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
    // The trailing edit that makes `build_paged_prompt_context` fall back to
    // the resident pipeline (`replacement: null` omits the target message).
    let edit = serde_json::json!({
        "type": "context_edit", "id": "ce000001", "parentId": parent,
        "timestamp": stamp, "targetId": first_user, "replacement": null,
    });
    let line = edit.to_string();
    file.write_all(line.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();
}

fn mock_model(context_window: u64) -> Model {
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

fn report(label: &str, base: &dhat::HeapStats) {
    let stats = dhat::HeapStats::get();
    println!(
        "[dhat] {label}: live={} (+{}) peak={} (+{}) blocks={}",
        stats.curr_bytes,
        stats.curr_bytes as i128 - base.curr_bytes as i128,
        stats.max_bytes,
        stats.max_bytes as i128 - base.curr_bytes as i128,
        stats.curr_blocks,
    );
}

fn message_text_bytes(messages: &[Message]) -> usize {
    let mut bytes = 0usize;
    for message in messages {
        let blocks = match message {
            Message::User(m) | Message::System(m) | Message::Developer(m) => &m.content,
            Message::Assistant(m) => &m.content,
            Message::ToolResult(m) => &m.content,
        };
        for block in blocks {
            if let ContentBlock::Text(text) = block {
                bytes += text.text.len();
            }
        }
    }
    bytes
}

/// A summarizer that serializes each request exactly like the provider's
/// streamed OpenAI body (the real per-request transient) and records the
/// largest body it built.
fn serializing_summarizer(max_body: Arc<std::sync::Mutex<usize>>) -> Arc<CompleteFn> {
    let model = mock_model(32_000);
    Arc::new(Box::new(move |context: Context| {
        let model = model.clone();
        let max_body = Arc::clone(&max_body);
        Box::pin(async move {
            let compat = OpenAiCompat::default();
            let options = StreamOptions::default();
            let body: OpenAiStreamedBody<'_> =
                build_openai_body_streamed(&model, &context, &options, &compat)
                    .map_err(|error| format!("body build failed: {error}"))?;
            let estimate = body.estimated_json_bytes();
            let mut payload: Vec<u8> = Vec::with_capacity(estimate);
            serde_json::to_writer(&mut payload, &body)
                .map_err(|error| format!("serialize failed: {error}"))?;
            let len = payload.len();
            {
                let mut largest = max_body.lock().unwrap();
                *largest = (*largest).max(len);
            }
            drop(payload);
            Ok(AssistantMessage {
                api: "openai-completions".into(),
                provider: "mock".into(),
                model: "mock-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "## Goal\npeak attribution summary".into(),
                    text_signature: None,
                })],
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

#[test]
#[ignore = "resident-compaction heap attribution harness; run explicitly with --nocapture"]
fn compaction_peak_attribution() {
    let size_mb: u64 = std::env::var("PIR_COMPACTION_HEAP_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    let dir = temp_dir("attr");
    let path = dir.join("big.jsonl");
    write_fixture(&path, size_mb * 1024 * 1024);
    let file_bytes = std::fs::metadata(&path).unwrap().len();
    println!("[dhat] fixture: {file_bytes} bytes");
    println!(
        "[dhat] sizes: FileEntry={} StoredMessage={} Message={} Context={}",
        std::mem::size_of::<pir::session_manager::FileEntry>(),
        std::mem::size_of::<pir::session_manager::StoredMessage>(),
        std::mem::size_of::<Message>(),
        std::mem::size_of::<Context>(),
    );

    let profile_path = dir.join("profile.json");
    let profiler = dhat::Profiler::builder().file_name(&profile_path).build();
    let base = dhat::HeapStats::get();
    println!(
        "[dhat] base: live={} blocks={}",
        base.curr_bytes, base.curr_blocks
    );

    // Phase 1: the print host's tail-first open.
    let mut manager =
        SessionManager::open_paged(&path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
            .expect("paged open");
    report("after paged open", &base);
    println!(
        "[dhat] resident entries={} paged={}",
        manager.resident_entries().len(),
        manager.is_paged()
    );

    // Phase 2: the resident widen (the context_edit fallback).
    manager.widen_to_full().expect("widen");
    report("after widen_to_full (file_entries)", &base);
    println!(
        "[dhat] resident entries={}",
        manager.resident_entries().len()
    );

    // Phase 3: the prompt context aggregate.
    let projection = manager.build_session_projection().expect("projection");
    report("after build_session_projection (Vec<Message>)", &base);
    println!(
        "[dhat] projection messages={} text_bytes={}",
        projection.messages.len(),
        message_text_bytes(&projection.messages)
    );

    // Phase 4: the cut basis. `print` moves the projection messages into the
    // loop context and keeps only per-entry metadata.
    let branch = manager.branch_path_meta(None);
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    let context = Context {
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: projection.messages,
        tools: None,
    };
    // Print keeps only the per-entry metadata snapshot; the projection's
    // entry handles are dropped before the compaction runs.
    drop(projection.entries);
    report("after Context { projection messages moved }", &base);
    let refs = ProjectionEntryMeta::cut_refs(&metas, &context.messages);
    report("after ProjectionEntryMeta::cut_refs", &base);

    // Phase 5: the borrowed preparation.
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 2_000,
        keep_recent_tokens: 4_000,
    };
    let model = mock_model(32_000);
    let preparation =
        compaction::prepare_compaction_refs(&refs, &branch, &config).expect("preparation");
    println!(
        "[dhat] preparation: summarize={} kept={} split={}",
        preparation.messages_to_summarize.len(),
        preparation.kept_messages.len(),
        preparation.is_split_turn,
    );
    report("after prepare_compaction_refs", &base);

    // Phase 6: the chunked summarizer over the borrowed span.
    let max_body = Arc::new(std::sync::Mutex::new(0usize));
    let complete = serializing_summarizer(Arc::clone(&max_body));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime
        .block_on(compaction::compact_with_preparation_refs(
            preparation,
            &model,
            complete,
            None,
        ))
        .expect("compaction");
    println!(
        "[dhat] summarizer largest request body={} B",
        *max_body.lock().unwrap()
    );
    report("after compact_with_preparation_refs", &base);

    // Phase 7: the rebuilt context (summary + retained tail).
    let rebuilt = compaction::rebuilt_context(&context, &result);
    report("after rebuilt_context", &base);
    println!(
        "[dhat] rebuilt messages={} text_bytes={}",
        rebuilt.messages.len(),
        message_text_bytes(&rebuilt.messages)
    );

    // Phase 8: persist the compaction entry while the pre-compaction context
    // is still alive (the print host's `apply_print_compaction_result`).
    let summary = result.summary.clone();
    let entry_id = manager
        .append_compaction_with_usage(
            summary,
            result.first_kept_entry_id.as_deref(),
            result.tokens_before,
            None,
            Some(result.usage),
            false,
        )
        .expect("append compaction");
    println!("[dhat] compaction entry id={entry_id}");
    report("after append_compaction_with_usage", &base);

    drop(rebuilt);
    drop(context);
    drop(result);
    drop(manager);
    report("after drop", &base);

    drop(profiler);
    println!("[dhat] profile: {}", profile_path.display());
    let _ = std::fs::remove_dir_all(&dir);
}
