//! PIR-BIN-053: reopening a session that has a stored `system` row must send
//! the model ONE system prompt.
//!
//! The transcript a resume replays contains a system entry describing the very
//! prompt the host re-renders from live options (a pi-written plain session, or
//! the checkpoint a compaction stores). Both copies used to reach the request:
//! upstream's `getCurrentSystemMessage` replays a stored system entry INTO the
//! head the same options build, so no duplicate can exist there; pir carries the
//! prompt out of band (`Context::system_prompt`) AND replays the stored entry
//! through the adapter's leading-system fold, concatenating the same text
//! twice.
//!
//! These tests assert the WIRE body the OpenAI-completions adapter builds for
//! exactly that resumed shape:
//!   * one leading system message,
//!   * one preamble (`You are an expert coding assistant`),
//!   * one `<cwd>` block,
//!   * the live rendering wins (the stored file's cwd must not survive), and
//!   * a genuinely different stored section/value still survives the fold.
//!
//! Also pinned: a session WITHOUT a stored system row still yields one prompt,
//! and the summarization request keeps its own system frame.

use std::collections::BTreeMap;

use pir::ai::api::openai::{build_openai_body, convert_messages};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    OpenAiCompat, StopReason, StreamOptions, TextContent, UserMessage, UserRole,
};
use pir::session_manager::SessionManager;
use serde_json::Value;

const PREAMBLE: &str = "preamble paragraph for PIR-BIN-053";
const CWD: &str = "C:/work/resumed";
const STORED_CWD: &str = "C:/work/old-cwd";

fn model() -> Model {
    Model {
        id: "gpt-test".into(),
        name: "GPT Test".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "https://example.invalid/v1".into(),
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
            tiers: vec![],
        },
        context_window: 16_000,
        max_tokens: 2_000,
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

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn assistant(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        api: "openai-completions".into(),
        provider: "test".into(),
        model: "gpt-test".into(),
        content: vec![text_block(text)],
        stop_reason: StopReason::Stop,
        timestamp: 2,
        ..AssistantMessage::default()
    })
}

/// A stored system entry shaped exactly like a pi-written session's leading
/// row (upstream `SystemMessage`: empty content, named sections) or like the
/// host's own recorded patch row.
fn stored_system(sections: &[(&str, &str)], timestamp: u64) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: None,
        tools_removed: None,
        sections: Some(
            sections
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Some((*value).to_owned())))
                .collect::<BTreeMap<_, _>>(),
        ),
        timestamp,
    })
}

/// The live rendering the host installs in `Context::system_prompt` — the same
/// text a pi/pir session records, but rendered for the CURRENT cwd.
fn live_prompt() -> String {
    format!("{PREAMBLE}\n\n<tools>\n(none)\n</tools>\n\n<cwd>\n{CWD}\n</cwd>")
}

/// The stored system entry of the resumed file (the same prompt, recorded with
/// the writer's cwd; the sections are keyed by name like upstream's).
fn stored_prompt_message() -> Message {
    stored_system(
        &[
            ("preamble", PREAMBLE),
            ("tools", "<tools>\n(none)\n</tools>"),
            ("cwd", &format!("<cwd>\n{STORED_CWD}\n</cwd>")),
        ],
        10,
    )
}

fn preamble_count(body: &Value) -> usize {
    let mut total = 0;
    for message in body["messages"].as_array().expect("messages array") {
        let Some(content) = message["content"].as_str() else {
            continue;
        };
        total +=
            content.matches("expert coding assistant").count() + content.matches(PREAMBLE).count();
    }
    total
}

fn cwd_count(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .map(|content| content.matches("<cwd>").count())
        .sum()
}

fn system_items(body: &Value) -> Vec<&Value> {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|message| message["role"] == serde_json::json!("system"))
        .collect()
}

// ---------------------------------------------------------------------------
// The resumed request
// ---------------------------------------------------------------------------

#[test]
fn reopened_session_with_a_stored_system_row_sends_one_prompt() {
    let context = Context {
        system_prompt: Some(live_prompt()),
        messages: vec![
            stored_prompt_message(),
            user("hello"),
            assistant("hi there"),
        ],
        tools: None,
    };
    let body = build_openai_body(
        &model(),
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");

    let systems = system_items(&body);
    assert_eq!(
        systems.len(),
        1,
        "exactly one leading system message, got {systems:#?}"
    );
    assert_eq!(
        preamble_count(&body),
        1,
        "the prompt must be sent once, not once live plus once replayed: {:#?}",
        body["messages"]
    );
    assert_eq!(
        cwd_count(&body),
        1,
        "one <cwd> block per request, got {:#?}",
        body["messages"]
    );
    let leading = systems[0]["content"].as_str().expect("system text");
    assert!(
        leading.starts_with(PREAMBLE),
        "the LIVE rendering must lead the request: {leading}"
    );
    assert!(
        leading.contains(CWD) && !leading.contains(STORED_CWD),
        "the live cwd wins; the stored file's cwd must not be replayed: {leading}"
    );
}

#[test]
fn reopened_session_without_a_stored_system_row_still_sends_one_prompt() {
    let context = Context {
        system_prompt: Some(live_prompt()),
        messages: vec![user("hello"), assistant("hi there")],
        tools: None,
    };
    let body = build_openai_body(
        &model(),
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(system_items(&body).len(), 1);
    assert_eq!(preamble_count(&body), 1);
    assert_eq!(cwd_count(&body), 1);
}

#[test]
fn a_genuinely_different_stored_section_still_reaches_the_request() {
    // Upstream patches sections BY NAME: a stored section the live rendering
    // does not carry is real extra instruction and must survive the fold.
    let context = Context {
        system_prompt: Some(live_prompt()),
        messages: vec![
            stored_prompt_message(),
            stored_system(&[("notes", "deployment uses the staging cluster")], 11),
            user("hello"),
        ],
        tools: None,
    };
    let body = build_openai_body(
        &model(),
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(preamble_count(&body), 1);
    assert_eq!(cwd_count(&body), 1);
    let converted = convert_messages(&model(), &context, &OpenAiCompat::default());
    let text = converted
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("deployment uses the staging cluster"),
        "a section the live prompt does not carry must be replayed: {text}"
    );
}

// ---------------------------------------------------------------------------
// The real resumed FILE (a compaction checkpoint in the session)
// ---------------------------------------------------------------------------

#[test]
fn reopening_a_compacted_session_file_sends_one_prompt() {
    let dir = std::env::temp_dir().join(format!(
        "pir-053-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let created = SessionManager::create(&CWD, &dir).expect("create session");
    let path = created.session_file().expect("session file").to_path_buf();
    drop(created);

    let mut session = SessionManager::open(&path).expect("open session");
    session.append_message(user("one")).expect("append user");
    session
        .append_message(assistant("payload"))
        .expect("append assistant");
    session
        .append_message(stored_prompt_message())
        .expect("append stored system row");
    // The checkpoint the host stores on compaction: the replayed system frame
    // (upstream `appendCompaction(..., getCurrentSystemMessage(projection))`).
    session
        .append_compaction_with_usage("checkpoint summary", None, 100, None, None, false)
        .expect("append compaction");
    session.append_message(user("two")).expect("append user");
    drop(session);

    // A NEW process opens the file: this is the resume case.
    let resumed = SessionManager::open(&path).expect("reopen session");
    let messages = resumed.build_session_messages();
    assert!(
        messages.iter().any(|message| match message {
            Message::System(system) => {
                system.sections.is_some()
                    || system.content.iter().any(|block| match block {
                        ContentBlock::Text(text) => !text.text.is_empty(),
                        _ => false,
                    })
            }
            _ => false,
        }),
        "the resumed transcript must carry the stored system row: {messages:#?}"
    );
    let context = Context {
        system_prompt: Some(live_prompt()),
        messages,
        tools: None,
    };
    let body = build_openai_body(
        &model(),
        &context,
        &StreamOptions::default(),
        &OpenAiCompat::default(),
    )
    .expect("body");
    assert_eq!(
        preamble_count(&body),
        1,
        "a resumed compacted session must send one prompt: {:#?}",
        body["messages"]
    );
    assert_eq!(cwd_count(&body), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
