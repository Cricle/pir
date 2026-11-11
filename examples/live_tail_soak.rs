//! PIR-SDK-018 live-tail soak: a bounded resident session over many rounds.
//!
//! Runs one long-lived agent against the in-process scripted provider with a
//! real tool call per round (the R44 support-bot payload shape: user,
//! assistant tool call, tool result, assistant = 4 messages/round), samples
//! this process's WorkingSet via `tasklist`, and prints one CSV row per
//! sample:
//!
//! ```text
//! cargo run --example live_tail_soak -- [rounds] [sample_every] [live|full] [compact_every]
//! ```
//!
//! `live` (default) opts into `live_session_tail(64 KiB, 128 entries)`;
//! `full` keeps the upstream whole-transcript residency. Same payload, same
//! provider, same sampling: apples-to-apples for the PIR-SDK-018 decision.
//! A non-zero `compact_every` (R51) runs manual `agent.compact()` at that
//! cadence and prints `COMPACT,round,...` rows (live-tail + compaction combo).
//!
//! Everything is offline and deterministic; the session directory is a fresh
//! temp dir per run.

mod common;

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, Context, ErrorReason, Model, StopReason, StreamOptions, TerminalReason,
    ThinkingLevel,
};
use pir::compaction::CompactionConfig;
use pir::sdk::{AgentBuilder, SessionMode};

/// Scripted replies WITHOUT request recording.
///
/// `common::ScriptedProvider` retains every `Context` it is sent (useful for
/// assertions, fatal for a RSS soak: it turns the host into an O(n^2)
/// transcript accumulator). This provider only pops the next reply, so the
/// measured RSS is the agent/session memory PIR-SDK-018 is about.
struct NoRecordProvider {
    replies: Mutex<VecDeque<AssistantMessage>>,
}

impl NoRecordProvider {
    fn registry(replies: Vec<AssistantMessage>) -> (Arc<ProviderRegistry>, Model) {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(Self {
            replies: Mutex::new(replies.into()),
        }));
        (Arc::new(registry), common::model())
    }
}

impl Provider for NoRecordProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn name(&self) -> &str {
        "no-record scripted provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![common::model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        // Compaction summarizer calls do not consume a scripted round reply.
        // The summarizer prompt opens with `<conversation>` (history) or
        // `# Conversation` (turn prefix); fall back to the summarizer system
        // prompt / tools-less context for robustness.
        let summarizer_prompt = common::user_texts(&context)
            .iter()
            .any(|text| text.starts_with("<conversation>") || text.starts_with("# Conversation"));
        if summarizer_prompt
            || context.tools.is_none()
            || context
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("context summarization assistant"))
        {
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                common::text_message("SUMMARY: the soak recorded the rounds before this cut."),
            ));
            return stream;
        }
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        match reply {
            Some(reply) => {
                let reason = match reply.stop_reason {
                    StopReason::ToolUse => TerminalReason::ToolUse,
                    StopReason::Length => TerminalReason::Length,
                    _ => TerminalReason::Stop,
                };
                stream.push(AssistantMessageEventStream::done_event(reason, reply));
            }
            None => {
                let mut error = common::text_message("no-record provider ran out of replies");
                error.stop_reason = StopReason::Error;
                error.error_message = Some("no-record provider ran out of replies".to_owned());
                stream.push(AssistantMessageEventStream::error_event(
                    ErrorReason::Error,
                    error,
                ));
            }
        }
        stream
    }
}

/// One deterministic tool call per round: the payload shape a support bot
/// writes per ticket (real tool lifecycle, 4 transcript messages/round).
struct SoakProbe;

impl AgentTool for SoakProbe {
    fn name(&self) -> &str {
        "soak_probe"
    }

    fn description(&self) -> &str {
        "Record the soak round and return it"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "round": { "type": "integer", "description": "Round number" }
            },
            "required": ["round"]
        }))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let round = args.get("round").and_then(Value::as_i64).unwrap_or(0);
        Box::pin(async move { ToolResult::ok(format!("round {round} recorded")) })
    }
}

fn rss_kib(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| line.contains(&pid.to_string()))?;
    let memory = line.rsplit("\",\"").next()?.trim_matches('"');
    let digits: String = memory.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn main() -> std::process::ExitCode {
    common::run_big_stack(async {
        if let Err(error) = run().await {
            eprintln!("error: {error:?}");
            return std::process::ExitCode::FAILURE;
        }
        std::process::ExitCode::SUCCESS
    })
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rounds: u64 = args
        .first()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    let sample_every: u64 = args
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(25);
    let mode = args.get(2).map(String::as_str).unwrap_or("live");
    let live = match mode {
        "live" => true,
        "full" => false,
        other => return Err(format!("mode must be `live` or `full`, got `{other}`").into()),
    };
    // R51: optional manual-compaction cadence (0 = no compaction). Compaction
    // summarizes the whole (paged) transcript and re-arms the live tail.
    let compact_every: u64 = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pir-live-tail-soak-{}-{mode}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sessions"))?;

    // Two provider requests per round: the tool call, then the final answer.
    // Summarizer requests are answered separately (see NoRecordProvider).
    let mut replies = Vec::with_capacity((rounds * 2) as usize);
    for round in 1..=rounds {
        replies.push(common::tool_calls_message(&[(
            "soak_probe",
            json!({ "round": round }),
        )]));
        replies.push(common::text_message("ack"));
    }
    let (registry, model) = NoRecordProvider::registry(replies);
    let mut builder = AgentBuilder::new()
        .with_registry(registry, model)
        .cwd(&dir)
        .system_prompt("You are a live-tail soak agent.")
        .session(SessionMode::Dir(dir.join("sessions")))
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(SoakProbe));
    if compact_every > 0 {
        // Manual-only: `enabled: false` keeps the threshold hook off; the
        // explicit `agent.compact()` below still uses these cut settings.
        builder = builder.compaction(CompactionConfig {
            enabled: false,
            reserve_tokens: 16_384,
            keep_recent_tokens: 1,
        });
    } else {
        builder = builder.no_compaction();
    }
    if live {
        builder = builder.live_session_tail(64 * 1024, 128);
    } else {
        builder = builder.without_live_session_tail();
    }
    let agent = builder.build().await?;

    let pid = std::process::id();
    println!(
        "PID={pid} rounds={rounds} sample_every={sample_every} mode={mode} budget={}",
        if live { "64KiB/128" } else { "unbounded" }
    );
    println!("CSV,round,rss_bytes,session_bytes,session_lines,transcript_messages,elapsed_ms");
    let started = std::time::Instant::now();
    sample(&agent, 0, started).await;
    for round in 1..=rounds {
        agent
            .prompt(format!(
                "soak turn {round} fixed payload ........................................"
            ))
            .await?;
        if compact_every > 0 && round % compact_every == 0 {
            let result = agent.compact().await?;
            let rss = rss_kib(pid).map(|kib| kib * 1024).unwrap_or(0);
            println!(
                "COMPACT,{round},{rss},{},{},{},{}",
                agent.session_stats().await.total_messages,
                result.tokens_before,
                result.first_kept_index,
                result.retained_tail.len()
            );
        }
        if round % sample_every == 0 || round == rounds {
            sample(&agent, round, started).await;
        }
    }
    println!(
        "SUMMARY,rounds={rounds},elapsed_ms={},final_rss_bytes={}",
        started.elapsed().as_millis(),
        rss_kib(pid).map(|kib| kib * 1024).unwrap_or(0)
    );
    // Whole-history read at the end proves the trimmed file is complete.
    let transcript = agent.transcript().await;
    println!("TRANSCRIPT,{}", transcript.len());
    let path = agent.session_path().expect("session path");
    println!("SESSION_FILE,{}", path.display());
    let content = std::fs::read_to_string(&path)?;
    let lines = content.lines().count();
    let compaction_rows = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row.get("type").and_then(Value::as_str) == Some("compaction"))
        .count();
    println!("FILE_LINES,{lines}");
    println!("COMPACTION_ROWS,{compaction_rows}");
    let live_transcript_len = transcript.len();
    agent.shutdown().await?;

    // Resume the compacted live-trimmed file through the public API: the
    // compaction projection (summary + retained tail) must replay completely.
    let (resume_registry, resume_model) =
        NoRecordProvider::registry(vec![common::text_message("ack")]);
    let resumed = AgentBuilder::new()
        .with_registry(resume_registry, resume_model)
        .cwd(&dir)
        .system_prompt("You are a live-tail soak agent.")
        .session(SessionMode::Existing(path.clone()))
        .live_session_tail(64 * 1024, 128)
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(SoakProbe))
        .build()
        .await?;
    let resumed_len = resumed.transcript().await.len();
    println!("RESUME_TRANSCRIPT,{resumed_len},expected={live_transcript_len}");
    resumed.prompt("after resume").await?;
    println!("RESUME_AFTER_PROMPT,{}", resumed.transcript().await.len());
    resumed.shutdown().await?;

    // Fork the compacted, live-trimmed session through the public runtime.
    let (fork_registry, fork_model) = NoRecordProvider::registry(Vec::new());
    let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
        cwd: Some(dir.clone()),
        agent_dir: Some(dir.join("agent")),
        model_registry: Some(pir::sdk::ModelRegistry::from_registry(fork_registry)),
        model: Some(fork_model),
        thinking_level: Some(ThinkingLevel::Off),
        session: Some(pir::session_manager::SessionManager::open(&path)?),
        no_tools: true,
        resource_loader: Some(
            pir::sdk::DefaultResourceLoader::new(dir.clone(), dir.join("agent"))
                .with_home(dir.clone())
                .without_skills()
                .without_prompt_templates(),
        ),
        ..Default::default()
    })
    .await?;
    let first_user_id = {
        let manager = created.session.session_manager().expect("session manager");
        let guard = manager.lock().await;
        guard
            .entries()
            .into_iter()
            .find_map(|entry| match entry {
                pir::session_manager::FileEntry::Message(message)
                    if message.message.role() == "user" =>
                {
                    Some(message.id.clone())
                }
                _ => None,
            })
            .expect("first user entry")
    };
    let runtime = pir::sdk::AgentSessionRuntime::new(
        Arc::clone(&created.session),
        dir.clone(),
        dir.join("agent"),
        Some(dir.join("sessions")),
    );
    let fork_before = created.session.session_path().expect("pre-fork path");
    match runtime
        .fork(&first_user_id, pir::agent_session::ForkPosition::Before)
        .await?
    {
        pir::agent_session::ForkEntryOutcome::Branched { selected_text } => println!(
            "FORK,ok,selected={:?},new={},parent={}",
            selected_text,
            created
                .session
                .session_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            fork_before.display()
        ),
        pir::agent_session::ForkEntryOutcome::Cancelled => println!("FORK,cancelled"),
    }
    std::io::stdout().flush()?;
    Ok(())
}

async fn sample(agent: &pir::sdk::Agent, round: u64, started: std::time::Instant) {
    let pid = std::process::id();
    let rss = rss_kib(pid).map(|kib| kib * 1024).unwrap_or(0);
    let path = agent.session_path();
    let (session_bytes, lines) = path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| (text.len() as u64, text.lines().count()))
        .unwrap_or((0, 0));
    let messages = agent.session_stats().await.total_messages;
    println!(
        "CSV,{round},{rss},{session_bytes},{lines},{messages},{}",
        started.elapsed().as_millis()
    );
}
