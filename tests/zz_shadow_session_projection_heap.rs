//! Lane-J2 heap attribution harness for the 2 MB TUI session startup delta.
//!
//! Ignored by default because it installs the `dhat` global allocator and
//! writes a DHAT JSON profile. Run explicitly:
//!
//! ```text
//! cargo test --test session_projection_heap --no-default-features \
//!     --features session,tui --locked -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The phases mirror the interactive host's startup:
//!
//! 1. `SessionManager::open_paged` (tail-first, the TUI's `--session` open);
//! 2. `build_session_projection` over the resident window (the attach replay's
//!    `AgentSession::transcript`);
//! 3. `transcript_entries_from_messages` (the TUI's `replay_transcript_entries`
//!    conversion into `ChatEntry`s);
//! 4. older-page load-more (`read_older_page` + conversion), dropped per page;
//! 5. `widen_to_full` + `build_session_projection` (the first prompt's request
//!    context), moved into `Context` exactly like `prompt_loop`.
//!
//! Each phase prints DHAT live/peak counters. The profile JSON keeps the
//! per-allocation-site stacks for the attribution table.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pir::ai::types::{Context, Message};
use pir::session_manager::{SessionManager, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES};
#[cfg(feature = "tui")]
use pir::tui::app::ChatEntry;
#[cfg(feature = "tui")]
use pir::tui::terminal::transcript_entries_from_messages;

// Pin the message slot sizes this harness attributes heap growth to. Boxing
// `deferred`/`nested_calls` measured 408/400/400 B on 64-bit (was 592/592/424).
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<pir::ai::types::Message>() == 408);
    assert!(std::mem::size_of::<pir::ai::types::AssistantMessage>() == 400);
    assert!(std::mem::size_of::<pir::ai::types::ToolResultMessage>() == 400);
};

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-session-projection-heap-{label}-{}-{stamp}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

/// Byte-shape-identical to `scripts/perf/big_session_probe.py:make_session`
/// (user/assistant text pairs, leading model/thinking bookkeeping).
fn write_fixture(path: &Path, target_bytes: u64) -> u64 {
    let payload = "lorem ipsum dolor sit amet consectetur adipiscing elit "
        .repeat(20)
        .trim()
        .to_owned();
    let stamp = "2026-01-01T00:00:00.000Z";
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create session"));
    let mut written = 0u64;
    let line = |entry: serde_json::Value, file: &mut std::io::BufWriter<std::fs::File>| {
        let mut json = serde_json::to_string(&entry).expect("serialize fixture entry");
        json.push('\n');
        file.write_all(json.as_bytes()).expect("write entry");
        json.len() as u64
    };
    written += line(
        serde_json::json!({"type": "session", "version": 3, "id": "fixture-session",
                           "timestamp": stamp, "cwd": "C:\\\\repo"}),
        &mut file,
    );
    written += line(
        serde_json::json!({"type": "model_change", "id": "m0", "parentId": serde_json::Value::Null,
                           "timestamp": stamp, "provider": "mock", "modelId": "mock-model"}),
        &mut file,
    );
    written += line(
        serde_json::json!({"type": "thinking_level_change", "id": "t0", "parentId": "m0",
                           "timestamp": stamp, "thinkingLevel": "off"}),
        &mut file,
    );
    let mut index = 0u64;
    let mut parent = "t0".to_owned();
    while written < target_bytes {
        index += 1;
        let user_id = format!("u{index:06}");
        let asst_id = format!("a{index:06}");
        let usage = serde_json::json!({
            "input": 1_500_000 + index * 250, "output": 120, "cacheRead": 0,
            "cacheWrite": 0, "totalTokens": 1_500_000 + index * 250 + 120,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
        });
        written += line(
            serde_json::json!({"type": "message", "id": user_id, "parentId": parent,
                               "timestamp": stamp,
                               "message": {"role": "user", "timestamp": 0,
                                           "content": [{"type": "text",
                                                        "text": format!("question {index}: {payload}")}]}}),
            &mut file,
        );
        written += line(
            serde_json::json!({"type": "message", "id": asst_id, "parentId": user_id,
                               "timestamp": stamp,
                               "message": {"role": "assistant", "timestamp": 0,
                                           "api": "openai-completions", "provider": "mock",
                                           "model": "mock-model", "stopReason": "stop",
                                           "usage": usage,
                                           "content": [{"type": "text",
                                                        "text": format!("answer {index}: {payload}")}]}}),
            &mut file,
        );
        parent = asst_id;
    }
    file.flush().expect("flush fixture");
    written
}

fn report(label: &str, before: &dhat::HeapStats, after: &dhat::HeapStats) {
    println!(
        "[dhat] {label}: live={:+} live_after={} peak_after={} blocks={:+}",
        after.curr_bytes as i128 - before.curr_bytes as i128,
        after.curr_bytes,
        after.max_bytes,
        after.curr_blocks as i128 - before.curr_blocks as i128,
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
            if let pir::ai::types::ContentBlock::Text(text) = block {
                bytes += text.text.len();
            }
        }
    }
    bytes
}

#[cfg(feature = "tui")]
fn chat_text_bytes(entries: &[ChatEntry]) -> usize {
    let mut bytes = 0usize;
    for entry in entries {
        match entry {
            ChatEntry::User(text) => bytes += text.len(),
            ChatEntry::Assistant(message) => {
                for block in &message.content {
                    if let pir::ai::types::ContentBlock::Text(text) = block {
                        bytes += text.text.len();
                    }
                }
            }
            ChatEntry::ToolExecution { output, .. } => bytes += output.len(),
            _ => {}
        }
    }
    bytes
}

#[test]
#[ignore = "lane-J2 heap attribution harness; run explicitly with --nocapture"]
fn session_projection_heap() {
    let size_mb: u64 = std::env::var("PIR_HEAP_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    let dir = temp_dir("attr");
    let path = dir.join("tui-big.jsonl");
    let written = write_fixture(&path, size_mb * 1024 * 1024);
    println!("[dhat] fixture: {written} bytes");
    println!(
        "[dhat] sizes: FileEntry={} StoredMessage={} Message={}",
        std::mem::size_of::<pir::session_manager::FileEntry>(),
        std::mem::size_of::<pir::session_manager::StoredMessage>(),
        std::mem::size_of::<Message>(),
    );
    #[cfg(feature = "tui")]
    println!("[dhat] size ChatEntry={}", std::mem::size_of::<ChatEntry>());

    let profile_path = dir.join("profile.json");
    let profiler = dhat::Profiler::builder().file_name(&profile_path).build();
    let base = dhat::HeapStats::get();
    println!(
        "[dhat] base: live={} blocks={}",
        base.curr_bytes, base.curr_blocks
    );

    // 1. TUI tail-first open.
    let page_bytes: usize = std::env::var("PIR_PAGE_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TAIL_PAGE_BYTES);
    let page_entries: usize = std::env::var("PIR_PAGE_ENTRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TAIL_PAGE_ENTRIES);
    let mut manager =
        SessionManager::open_paged(&path, page_bytes, page_entries).expect("paged open");
    let after_open = dhat::HeapStats::get();
    report("after paged open", &base, &after_open);
    println!(
        "[dhat] resident entries={} paged={}",
        manager.resident_entries().len(),
        manager.is_paged()
    );

    // 2. Attach replay projection over the resident window.
    let projection = manager.build_session_projection().expect("projection");
    let after_projection = dhat::HeapStats::get();
    report("after resident projection", &after_open, &after_projection);
    println!(
        "[dhat] projection messages={} text_bytes={}",
        projection.messages.len(),
        message_text_bytes(&projection.messages)
    );

    // 3. The TUI conversion (`replay_transcript_entries`), when this test is
    // built with the `tui` feature.
    #[cfg(feature = "tui")]
    let after_chat = {
        let chat = transcript_entries_from_messages(projection.messages);
        let after_chat = dhat::HeapStats::get();
        report("after chat conversion", &after_projection, &after_chat);
        println!(
            "[dhat] chat entries={} text_bytes={}",
            chat.len(),
            chat_text_bytes(&chat)
        );
        drop(chat);
        after_chat
    };
    #[cfg(not(feature = "tui"))]
    let after_chat = {
        drop(projection.messages);
        dhat::HeapStats::get()
    };

    // 4. Older-page load-more, one bounded page at a time.
    let mut cursor = manager.prefix_cursor();
    let mut pages = 0usize;
    let mut page_messages = 0usize;
    while let Some(at) = cursor {
        let Some(page) = manager
            .read_older_page(
                at,
                pir::session_manager::DEFAULT_TAIL_PAGE_BYTES,
                pir::session_manager::DEFAULT_TAIL_PAGE_ENTRIES,
            )
            .expect("read older page")
        else {
            break;
        };
        let messages: Vec<Message> = page
            .entries()
            .iter()
            .flat_map(|entry| {
                pir::session_manager::projection::session_entry_to_context_messages(entry)
            })
            .collect();
        page_messages += messages.len();
        #[cfg(feature = "tui")]
        {
            let entries = transcript_entries_from_messages(messages);
            drop(entries);
        }
        #[cfg(not(feature = "tui"))]
        drop(messages);
        pages += 1;
        let next = page.page_start();
        cursor = if next < at { Some(next) } else { None };
    }
    let after_pages = dhat::HeapStats::get();
    report("after load-more pages", &after_chat, &after_pages);
    println!("[dhat] load-more pages={pages} page_messages={page_messages}");

    // 5. First prompt: widen + full projection moved into the request context.
    manager.widen_to_full().expect("widen");
    let after_widen = dhat::HeapStats::get();
    report("after widen_to_full", &after_pages, &after_widen);
    let full = manager.build_session_projection().expect("full projection");
    let after_full = dhat::HeapStats::get();
    report("after full projection", &after_widen, &after_full);
    let context = Context {
        system_prompt: Some("You are a helpful assistant.".to_owned()),
        messages: full.messages,
        tools: None,
    };
    let after_context = dhat::HeapStats::get();
    report("after request context", &after_full, &after_context);
    drop(context);
    drop(manager);
    let final_stats = dhat::HeapStats::get();
    report("final live", &base, &final_stats);

    drop(profiler);
    println!("[dhat] profile: {}", profile_path.display());
    assert!(profile_path.exists(), "dhat profile was not written");
}
