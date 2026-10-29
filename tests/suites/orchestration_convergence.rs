//! Tests for the orchestration convergence work:
//!
//! - TUI session-plan attach wiring: a resolved [`SessionPlan`] opened via
//!   `open_session_manager` and handed to [`AgentSession`] carries the prior
//!   transcript into the first model request (what the interactive worker
//!   now does instead of hand-building a stateless Context);
//! - rpc writer-death regression: a dead output writer ends the serve loop
//!   with an error instead of silently dropping every future response;
//! - compaction alignment with upstream: `readFiles`/`modifiedFiles`
//!   tracking in the summary, and `firstKeptEntryId` persistence + replay.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::coding_agent::session_resolve::{open_session_manager, SessionPlan};
use pir::compaction::CompactionConfig;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fakes (same scripted-provider pattern as tests/agent_session.rs)
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
        // Large window so the threshold compaction hook never fires during
        // ordinary prompts in these tests.
        context_window: 1_000_000,
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

fn tool_call(id: &str, name: &str, path: &str) -> ContentBlock {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_owned(), serde_json::json!(path));
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn assistant(content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-orchestration-{name}-{}",
        pir::ai::util::now_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build the same shape of `AgentSessionConfig` the interactive worker uses
/// (`build_agent_session` in `src/tui/terminal.rs`).
fn worker_like_session(
    registry: Arc<ProviderRegistry>,
    session: Option<SessionManager>,
    compaction: Option<CompactionConfig>,
) -> AgentSession {
    let mut tools = pir::agent::tool_execution::ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut tools);
    AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Medium,
        tools,
        session,
        compaction,
        max_turns: 100,
        system_prompt: None,
        cwd: None,
        event_stream: None,
        extensions: None,
        tool_selection: None,
    })
}

// ---------------------------------------------------------------------------
// Session plan -> AgentSession attach wiring (TUI worker convergence)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_plan_attaches_prior_transcript_into_agent_session() {
    let dir = temp_dir("attach-plan");
    let session_dir = dir.join("sessions");

    // Seed a persisted session with one prior turn.
    let mut seeded = SessionManager::create("C:\\repo", &session_dir).unwrap();
    seeded
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block("seed question from the earlier session")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .unwrap();
    let path = seeded.path().to_path_buf();

    // The TUI worker path: resolved plan -> open_session_manager -> config.
    let plan = SessionPlan::Open(path.clone());
    let manager = open_session_manager(&plan, "C:\\repo", &session_dir)
        .unwrap()
        .expect("an Open plan yields session storage");
    assert_eq!(manager.id(), seeded.id());

    let captured: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let registry = fake_registry(Arc::new(move |_index, context: &Context| {
        sink.lock().unwrap().push(context.clone());
        assistant(vec![text_block("reply")])
    }));
    let session = worker_like_session(registry, Some(manager), None);
    let state = session.state();
    assert_eq!(state.session_path.as_deref(), Some(path.to_str().unwrap()));
    assert_eq!(state.session_id.as_deref(), Some(seeded.id()));

    session
        .prompt("next question", Arc::new(|_| {}))
        .await
        .unwrap();
    let contexts = captured.lock().unwrap();
    assert!(
        contexts
            .iter()
            .any(|context| context.messages.iter().any(|message| {
                pir::compaction::serialize_conversation(std::slice::from_ref(message))
                    .contains("seed question from the earlier session")
            })),
        "the attached session's prior transcript must reach the model request"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ephemeral_session_plan_keeps_transcript_in_memory() {
    let dir = temp_dir("attach-ephemeral");
    let session_dir = dir.join("sessions");
    let plan = SessionPlan::Ephemeral { id: None };
    let manager = open_session_manager(&plan, "C:\\repo", &session_dir).unwrap();
    assert!(manager.is_none(), "--no-session must not open storage");

    let session = worker_like_session(
        fake_registry(Arc::new(|_, _| assistant(vec![]))),
        manager,
        None,
    );
    let state = session.state();
    assert!(state.session_id.is_none());
    assert!(state.session_path.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// rpc writer-death regression (D3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_serve_ends_when_output_writer_dies() {
    let dir = temp_dir("rpc-writer-death");
    let factory: SessionFactory = Arc::new(|_| Err(anyhow::anyhow!("factory not used")));
    let server = RpcServer::new(dir.clone(), factory);

    // Two pipes: commands flow client -> server, responses server -> client.
    // The response pipe's client end is a whole DuplexStream, so dropping it
    // makes every server-side write fail with BrokenPipe.
    let (mut client_in, server_in) = tokio::io::duplex(1024);
    let (client_out, server_out) = tokio::io::duplex(1024);

    let mut serve = tokio::spawn(async move { server.serve(server_in, server_out).await });

    // Handshake works: the hello response comes back.
    use tokio::io::AsyncWriteExt;
    client_in
        .write_all(b"{\"type\":\"hello\"}\n")
        .await
        .unwrap();
    client_in.flush().await.unwrap();
    let mut reader = tokio::io::BufReader::new(client_out);
    let hello = tokio::time::timeout(Duration::from_secs(5), async {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    })
    .await
    .expect("hello response within timeout");
    assert!(
        hello.contains("\"hello\""),
        "expected hello response: {hello}"
    );

    // Kill the output side: dropping the client end of the response pipe
    // makes the server's writer task fail on its next write, and its channel
    // receiver drops with it.
    drop(reader);

    // Now every response can no longer be delivered. Keep issuing commands
    // until the serve loop notices the dead writer and ends (instead of
    // silently dropping responses forever).
    let mut finished = false;
    for _ in 0..100 {
        client_in.write_all(b"{\"type\":\"list\"}\n").await.unwrap();
        client_in.flush().await.unwrap();
        match tokio::time::timeout(Duration::from_millis(50), &mut serve).await {
            Ok(joined) => {
                let result = joined.expect("serve task joins");
                assert!(
                    result.is_err(),
                    "serve must report the dead writer as an error, got {result:?}"
                );
                finished = true;
                break;
            }
            Err(_) => continue,
        }
    }
    assert!(
        finished,
        "serve must end once the output writer is dead (regression: it used to keep reading stdin and silently dropping responses)"
    );

    drop(client_in);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Compaction alignment (D4): file tracking + firstKeptEntryId
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compaction_tracks_read_and_modified_files() {
    // Two turns: turn 1 carries read/write/edit tool calls and is summarized;
    // turn 2 (plain text) is the retained tail. keep_recent_tokens: 1 forces
    // the cut back to turn 2's user message.
    let tool_result = |call_id: &str, tool_name: &str, text: &str| {
        Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: call_id.into(),
            tool_name: tool_name.into(),
            content: vec![text_block(text)],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: None,
            nested_calls: None,
            is_error: false,
            duration_ms: None,
            timestamp: 1,
        })
    };
    let messages = vec![
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block("do the thing")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }),
        Message::Assistant(assistant(vec![
            tool_call("c1", "read", "src/a.ts"),
            tool_call("c2", "write", "src/b.ts"),
            tool_call("c3", "edit", "src/c.ts"),
        ])),
        tool_result("c1", "read", "contents of a"),
        tool_result("c2", "write", "wrote b"),
        tool_result("c3", "edit", "edited c"),
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block("and now the second turn")],
            tools_added: None,
            tools_removed: None,
            timestamp: 2,
            sections: None,
        }),
        Message::Assistant(assistant(vec![text_block("done")])),
    ];

    let model = fake_model();
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 16_384,
        keep_recent_tokens: 1,
    };
    let complete: pir::compaction::CompleteFn =
        Box::new(|_| Box::pin(async { Ok(assistant(vec![text_block("the structured summary")])) }));
    let result = pir::compaction::compact_context(
        Context {
            system_prompt: None,
            messages,
            tools: None,
        },
        &model,
        Arc::new(complete),
        &config,
        None,
        None,
    )
    .await
    .expect("compaction succeeds");

    // b.ts was written (so no longer read-only); a.ts read-only; c.ts edited.
    assert_eq!(result.read_files, vec!["src/a.ts".to_string()]);
    assert_eq!(
        result.modified_files,
        vec!["src/b.ts".to_string(), "src/c.ts".to_string()]
    );
    assert!(
        result
            .summary
            .contains("<read-files>\nsrc/a.ts\n</read-files>"),
        "summary must carry the read-files section: {}",
        result.summary
    );
    assert!(
        result
            .summary
            .contains("<modified-files>\nsrc/b.ts\nsrc/c.ts\n</modified-files>"),
        "summary must carry the modified-files section: {}",
        result.summary
    );
}

#[test]
fn first_kept_entry_id_persists_and_replays_from_entry_tree() {
    let dir = temp_dir("first-kept-id");
    let mut session = SessionManager::create("C:\\repo", &dir).unwrap();

    let user_message = |text: &str| {
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text_block(text)],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        })
    };
    session.append_message(user_message("m1")).unwrap();
    session.append_message(user_message("m2")).unwrap();
    session.append_message(user_message("m3")).unwrap();
    session.append_message(user_message("m4")).unwrap();

    // Cut point: keep m3 onward (upstream firstKeptEntryId = m3's entry id).
    let with_ids = session.build_context_entries_with_ids().unwrap();
    assert_eq!(with_ids.len(), 4);
    let m3_id = with_ids[2].0.clone().expect("message entries carry ids");

    session
        .append_compaction(
            "earlier turns summarized",
            Some(&m3_id),
            1234,
            Some(serde_json::json!({
                "readFiles": ["src/a.ts"],
                "modifiedFiles": ["src/b.ts"],
            })),
        )
        .unwrap();
    session.append_message(user_message("m5")).unwrap();

    // Replay: summary + entries from firstKeptEntryId + post-compaction.
    let context = session.build_context_entries().unwrap();
    let texts: Vec<String> = context
        .iter()
        .map(|message| match message {
            Message::User(user) => match &user.content[0] {
                ContentBlock::Text(text) => text.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        })
        .collect();
    assert_eq!(texts.len(), 4);
    assert!(texts[0].contains("earlier turns summarized"));
    assert_eq!(
        &texts[1..],
        &["m3".to_owned(), "m4".to_owned(), "m5".to_owned()]
    );

    // The persisted wire shape carries upstream `firstKeptEntryId`.
    let entries = session.entries();
    let compaction = entries
        .iter()
        .find_map(|entry| match entry {
            pir::session_manager::FileEntry::Compaction(inner) => Some(inner.clone()),
            _ => None,
        })
        .expect("compaction entry persisted");
    assert_eq!(compaction.first_kept_entry_id, m3_id);
    let wire = serde_json::to_value(&compaction).unwrap();
    assert_eq!(wire["firstKeptEntryId"], serde_json::json!(m3_id));
    assert_eq!(wire["tokensBefore"], serde_json::json!(1234));
    assert_eq!(
        wire["details"]["readFiles"],
        serde_json::json!(["src/a.ts"])
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agent_session_compact_persists_first_kept_entry_id() {
    let dir = temp_dir("compact-mapping");
    let seed_dir = dir.join("seed");
    let seeded = SessionManager::create("C:\\repo", &seed_dir).unwrap();
    let path = seeded.path().to_path_buf();

    // Two prompts with tiny messages (1 estimated token each), then a manual
    // compaction with keep_recent_tokens: 2: the budget is crossed at the
    // second turn's user message (the last assistant message is smaller than
    // the budget), which becomes the cut point (no split turn), so exactly
    // one summarizer call runs.
    let registry = fake_registry(Arc::new(|index, _| {
        assistant(vec![text_block(match index {
            0 => "first answer",
            1 => "ok",
            _ => "merged summary",
        })])
    }));
    let compaction = CompactionConfig {
        enabled: true,
        reserve_tokens: 16_384,
        keep_recent_tokens: 2,
    };
    let session = worker_like_session(registry, Some(seeded), Some(compaction));
    session.prompt("q1", Arc::new(|_| {})).await.unwrap();
    session.prompt("q2", Arc::new(|_| {})).await.unwrap();

    let result = session.compact(None).await.expect("manual compaction");
    // Flat message index of the cut point: the first prompt persists a
    // tool-declaration system entry before its assistant reply
    // (declareToolChanges), so the second turn's user message sits at flat
    // index 3 (declaration + q1 + first reply).
    assert_eq!(result.first_kept_index, 3, "cut lands on the second turn");

    // The persisted compaction entry carries the mapped entry id: the first
    // kept message is the second turn's user message.
    let reopened = SessionManager::open(&path).unwrap();
    let entries = reopened.entries();
    let compaction_entry = entries
        .iter()
        .find_map(|entry| match entry {
            pir::session_manager::FileEntry::Compaction(inner) => Some(inner.clone()),
            _ => None,
        })
        .expect("compaction persisted");
    let user2_id = reopened
        .build_context_entries_with_ids()
        .unwrap()
        .into_iter()
        .find(|(id, message)| {
            id.is_some()
                && matches!(
                    message,
                    Message::User(user) if matches!(&user.content[0],
                        ContentBlock::Text(text) if text.text.contains("q2"))
                )
        })
        .and_then(|(id, _)| id)
        .expect("second turn user entry present");
    assert_eq!(compaction_entry.first_kept_entry_id, user2_id);

    // Replay from the entry tree: [summary user message] + the kept second
    // turn (the compaction entry contributes only its summary when the entry
    // tree is used — never a stored retained tail as well). The user-message
    // filter below surfaces summary + q2 (the kept assistant reply is there
    // too but is not a User message).
    // Replay from the entry tree: the compaction entry replays the system
    // message captured at compaction time ahead of the summary (upstream
    // session-manager.ts: `entry.systemMessage ? [entry.systemMessage, summary]
    // : [summary]`), and upstream `getCurrentSystemMessage` is defined whenever
    // ANY system-role message exists — here the turn-1 tool declaration (empty
    // content) — so the replay leads with an empty system message. That entry
    // is part of the compaction entry itself, NOT part of the kept range: the
    // kept tail never re-includes declaration entries (upstream
    // `buildContextEntries` drops system-role messages from the kept range).
    let context = reopened.build_context_entries().unwrap();
    assert_eq!(
        context.len(),
        4,
        "system replay + summary + q2 + kept reply, got: {context:#?}"
    );
    assert!(
        matches!(&context[0], Message::System(system)
            if system.content.iter().all(|block| matches!(
                block, ContentBlock::Text(text) if text.text.is_empty()))),
        "the compaction entry replays its captured system message first"
    );
    let texts: Vec<String> = context
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => match &user.content[0] {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 2, "summary + q2: {texts:?}");
    // The replayed summary is the stored raw summary text, framed like
    // upstream's compactionSummary message.
    assert!(
        texts[0].contains("merged summary")
            && texts[0].starts_with("The conversation history before this point was compacted"),
        "summary text: {}",
        texts[0]
    );
    assert_eq!(texts[1], "q2", "texts: {texts:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Drift script parse step (pure batch; the batch itself is verified manually)
// ---------------------------------------------------------------------------

#[test]
fn migration_info_parse_step_matches_drift_script_expectations() {
    // The batch parses `key=value` lines from .migration-info; assert the
    // recorded commit parses and looks like a full 40-char hex hash (the
    // same validation the script applies via `RECORDED_COMMIT:~7,1`).
    let content = std::fs::read_to_string(".migration-info").expect(".migration-info readable");
    let mut commit = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("commit=") {
            commit = Some(rest.trim().to_owned());
        }
    }
    let commit = commit.expect("commit= line present");
    assert_eq!(commit.len(), 40, "recorded commit is a full sha1: {commit}");
    assert!(
        commit.chars().all(|c| c.is_ascii_hexdigit()),
        "recorded commit is hex: {commit}"
    );
    // The script's substring check succeeds for full hashes.
    assert!(!commit[..8].is_empty());
}
