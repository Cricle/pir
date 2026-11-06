//! Lane-E streamed-widen peak attribution (ignored by default).
//!
//! Builds the same ~100 MB linear v3 fixture as
//! `tests/lane_e_widen_attribution.rs`, then measures the NEW streaming path:
//! `SessionMessageSource` (file index + per-entry reads) feeding
//! `OpenAiStreamedBody` straight into a temp file. No `widen_to_full`, no
//! `Vec<Message>` projection.
//!
//! The measured phase is first in the process, so DHAT's `max_bytes` is the
//! streamed path's own heap peak. The resident-slice body is then built from
//! the existing `build_paged_prompt_context` output and both request bodies are
//! compared byte for byte.
//!
//! Run explicitly:
//! ```text
//! cargo test --test lane_e_streamed_peak -- --ignored --nocapture --test-threads=1
//! ```

use std::io::{BufWriter, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use dhat::HeapStats;

use pir::ai::api::openai::{build_openai_body_streamed, SessionMessageSource};
use pir::ai::types::{
    ContentBlock, Context, MessageSource as _, Model, ModelCost, ModelCostRates, StreamOptions,
    TextContent, UserMessage, UserRole,
};
use pir::session_manager::{SessionManager, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES};

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-lane-e-streamed-peak-{label}-{}-{stamp}",
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
            "type": "session", "version": 3, "id": "lane-e-peak",
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
        let user = serde_json::json!({
            "type": "message", "id": user_id, "parentId": parent, "timestamp": stamp,
            "message": {"role": "user", "timestamp": 0,
                "content": [{"type": "text", "text": format!("question {index}: {payload}")}]},
        });
        let asst = serde_json::json!({
            "type": "message", "id": asst_id, "parentId": user_id, "timestamp": stamp,
            "message": {"role": "assistant", "timestamp": 0,
                "api": "openai-completions", "provider": "mock", "model": "mock-model",
                "stopReason": "stop",
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

fn serialize_body_to_file(body: &impl serde::Serialize, path: &Path) -> u64 {
    let file = std::fs::File::create(path).expect("body file");
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    serde_json::to_writer(&mut writer, body).expect("serialize body");
    writer.flush().expect("flush body");
    std::fs::metadata(path).expect("body metadata").len()
}

fn files_equal(left: &Path, right: &Path) -> bool {
    let left_len = std::fs::metadata(left).expect("left metadata").len();
    let right_len = std::fs::metadata(right).expect("right metadata").len();
    if left_len != right_len {
        return false;
    }
    let mut left = std::io::BufReader::new(std::fs::File::open(left).expect("left open"));
    let mut right = std::io::BufReader::new(std::fs::File::open(right).expect("right open"));
    let mut left_chunk = vec![0u8; 1024 * 1024];
    let mut right_chunk = vec![0u8; 1024 * 1024];
    loop {
        let read = left.read(&mut left_chunk).expect("left read");
        if read == 0 {
            return right.read(&mut right_chunk).expect("right read") == 0;
        }
        right
            .read_exact(&mut right_chunk[..read])
            .expect("right read");
        if left_chunk[..read] != right_chunk[..read] {
            return false;
        }
    }
}

#[test]
#[ignore = "lane-E streamed-widen peak harness; run explicitly with --nocapture"]
fn lane_e_streamed_widen_peak() {
    let dir = temp_dir("peak");
    let path = dir.join("big.jsonl");
    write_fixture(&path, 100 * 1024 * 1024);
    println!(
        "[streamed-peak] fixture: {} bytes",
        std::fs::metadata(&path).unwrap().len()
    );

    let profile_path = dir.join("profile.json");
    let profiler = dhat::Profiler::builder().file_name(&profile_path).build();
    let base = HeapStats::get();
    println!(
        "[streamed-peak] base: live={} blocks={}",
        base.curr_bytes, base.curr_blocks
    );

    // ---- Streamed path (first in the process: max_bytes is its peak) ----
    let streamed_started = Instant::now();
    let mut manager =
        SessionManager::open_paged(&path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
            .expect("paged open");
    manager
        .append_message(user_message("Reply with OK"))
        .expect("append user");

    let source_started = Instant::now();
    let source = SessionMessageSource::build(&manager, manager.leaf_id()).expect("source");
    let source_elapsed = source_started.elapsed();
    let source_entries = source.entry_count();
    let source_summary = source.summary().expect("summary");
    println!(
        "[streamed-peak] source: entries={} summary_messages={} approx_bytes={}",
        source_entries,
        source_summary.message_count,
        source.approximate_json_bytes(),
    );

    let model = mock_model();
    let compat = pir::ai::types::OpenAiCompat::default();
    let options = StreamOptions {
        message_source: Some(Arc::new(source)),
        ..Default::default()
    };
    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_owned()),
        messages: Vec::new(),
        tools: None,
    };
    let body =
        build_openai_body_streamed(&model, &context, &options, &compat).expect("streamed body");
    let estimate = body.estimated_json_bytes();
    let streamed_body_path = dir.join("streamed-body.json");
    let streamed_bytes = serialize_body_to_file(&body, &streamed_body_path);
    drop(body);
    drop(options);
    drop(context);
    let streamed_elapsed = streamed_started.elapsed();
    drop(manager);

    let after_streamed = HeapStats::get();
    let streamed_peak = after_streamed.max_bytes.saturating_sub(base.curr_bytes);
    println!(
        "[streamed-peak] streamed: peak={} ({:.2} MB) live_after={} estimated_body={} body={} source_wall={:?} total_wall={:?}",
        streamed_peak,
        streamed_peak as f64 / (1024.0 * 1024.0),
        after_streamed.curr_bytes,
        estimate,
        streamed_bytes,
        source_elapsed,
        streamed_elapsed,
    );

    // ---- Resident-slice baseline (existing paged builder) ----
    let baseline_started = Instant::now();
    let mut manager =
        SessionManager::open_paged(&path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
            .expect("paged open 2");
    manager
        .append_message(user_message("Reply with OK"))
        .expect("append user 2");
    let paged = manager
        .build_paged_prompt_context()
        .expect("paged context")
        .expect("linear session");
    let paged_messages = paged.messages.len();
    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_owned()),
        messages: paged.messages,
        tools: None,
    };
    let options = StreamOptions::default();
    let baseline =
        build_openai_body_streamed(&model, &context, &options, &compat).expect("baseline body");
    let baseline_body_path = dir.join("baseline-body.json");
    let baseline_bytes = serialize_body_to_file(&baseline, &baseline_body_path);
    drop(baseline);
    let baseline_elapsed = baseline_started.elapsed();
    drop(context);
    drop(manager);

    println!(
        "[streamed-peak] baseline: messages={} body={} wall={:?}",
        paged_messages, baseline_bytes, baseline_elapsed
    );
    let identical = files_equal(&streamed_body_path, &baseline_body_path);
    println!(
        "[streamed-peak] byte-identical={} ({streamed_bytes} vs {baseline_bytes})",
        identical
    );
    assert!(identical, "streamed and resident bodies must be identical");
    let under_budget = streamed_peak < 100 * 1024 * 1024;
    println!(
        "[streamed-peak] under_100MB={under_budget} ({} bytes)",
        streamed_peak
    );
    assert!(under_budget, "streamed heap peak must stay under 100 MB");

    let final_stats = HeapStats::get();
    println!(
        "[streamed-peak] final: live={} peak={} blocks={}",
        final_stats.curr_bytes, final_stats.max_bytes, final_stats.curr_blocks
    );
    drop(profiler);
    println!("[streamed-peak] profile: {}", profile_path.display());
    let _ = std::fs::remove_dir_all(dir);
}
