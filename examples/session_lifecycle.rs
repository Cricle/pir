//! Session lifecycle across real process restarts: start, continue, fork,
//! inspect.
//!
//! Each subcommand is a separate process; the session file on disk is the
//! only state carried between them.
//!
//! ```text
//! cargo run --example session_lifecycle --no-default-features --features "sdk,session" -- start <root>
//! cargo run --example session_lifecycle --no-default-features --features "sdk,session" -- continue <root>
//! cargo run --example session_lifecycle --no-default-features --features "sdk,session" -- fork <root>
//! cargo run --example session_lifecycle --no-default-features --features "sdk,session" -- inspect <session.jsonl>
//! ```

mod common;

use std::path::{Path, PathBuf};

use pir::ai::types::ThinkingLevel;
use pir::sdk::{Agent, AgentBuilder, SessionManager, SessionMode};

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
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    match mode {
        "start" => start(Path::new(&args[2])).await?,
        "continue" => continue_session(Path::new(&args[2])).await?,
        "fork" => fork(Path::new(&args[2])).await?,
        "inspect" => inspect(Path::new(&args[2]))?,
        other => return Err(format!("unknown mode {other:?}").into()),
    }
    Ok(())
}

fn project_dir(root: &Path) -> PathBuf {
    root.join("project")
}

fn session_dir(root: &Path) -> PathBuf {
    root.join("sessions")
}

async fn build_agent(root: &Path, replies: Vec<pir::ai::types::AssistantMessage>) -> Agent {
    let (registry, model) = common::ScriptedProvider::registry(replies);
    AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .cwd(project_dir(root))
        .session(SessionMode::Dir(session_dir(root)))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build the agent")
}

async fn start(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(project_dir(root))?;
    std::fs::create_dir_all(session_dir(root))?;
    let agent = build_agent(root, vec![common::text_message("Noted: the number is 41.")]).await;
    let outcome = agent.prompt("Remember the number 41.").await?;
    println!("answer: {}", outcome.message_text().unwrap_or_default());
    let path = agent.session_path().expect("a persistent session path");
    let id = agent.session_id().expect("a session id");
    let _ = agent.set_session_name(Some("lifecycle")).await?;
    std::fs::write(root.join("original.txt"), path.display().to_string())?;
    println!("session path: {}", path.display());
    println!("session id: {id}");
    println!("session name: {:?}", agent.session_name().await);
    println!("usage totals: {:?}", agent.usage_totals());
    agent.shutdown().await?;
    Ok(())
}

async fn continue_session(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let original: PathBuf = std::fs::read_to_string(root.join("original.txt"))?
        .trim()
        .into();
    let (registry, model, requests) =
        common::ScriptedProvider::registry_recording(vec![common::text_message(
            "The number was 41; plus one is 42.",
        )]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .cwd(project_dir(root))
        .session(SessionMode::Dir(session_dir(root)))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    // No explicit path: `continue_session` resolves the most recent session
    // for this cwd from the directory, exactly like `pir --continue`.
    agent.continue_session().await?;
    println!("continued path: {:?}", agent.session_path());
    println!("continued id: {:?}", agent.session_id());
    assert_eq!(
        agent.session_path().as_deref(),
        Some(original.as_path()),
        "continue must reopen the most recent session for this cwd"
    );
    let transcript_before = agent.transcript().await;
    println!(
        "transcript before prompt: {} messages",
        transcript_before.len()
    );

    let outcome = agent
        .prompt("What number did I ask you to remember? Add one.")
        .await?;
    println!("answer: {}", outcome.message_text().unwrap_or_default());
    let first = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("one recorded request");
    println!("request user texts: {:?}", common::user_texts(&first));
    assert!(
        common::user_texts(&first)
            .iter()
            .any(|text| text.contains("Remember the number 41.")),
        "the prior user message must survive the process restart"
    );
    println!(
        "transcript after prompt: {} messages",
        agent.transcript().await.len()
    );
    agent.shutdown().await?;
    Ok(())
}

async fn fork(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let original: PathBuf = std::fs::read_to_string(root.join("original.txt"))?
        .trim()
        .into();
    let fork_dir = root.join("forks");
    std::fs::create_dir_all(&fork_dir)?;
    let cwd = project_dir(root).to_string_lossy().into_owned();
    let forked = SessionManager::fork_from_source(&original, &cwd, &fork_dir, None)?;
    let fork_path = forked.path().to_path_buf();
    let fork_id = forked.id().to_owned();
    println!("fork path: {}", fork_path.display());
    println!("fork id: {fork_id}");

    let agent = build_agent(
        root,
        vec![common::text_message("This is the forked branch answering.")],
    )
    .await;
    agent.attach_session(&fork_path).await?;
    println!(
        "attached fork transcript: {} messages",
        agent.transcript().await.len()
    );
    let outcome = agent.prompt("Answer only on the forked branch.").await?;
    println!("answer: {}", outcome.message_text().unwrap_or_default());
    assert_eq!(agent.session_id().as_deref(), Some(fork_id.as_str()));
    assert_eq!(agent.session_path().as_deref(), Some(fork_path.as_path()));
    agent.shutdown().await?;
    Ok(())
}

/// Validate the on-disk upstream shape: a session header first, then entries
/// with typed payloads. Print every distinct row type and the header keys.
fn inspect(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let rows: Vec<serde_json::Value> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let header = &rows[0];
    println!("rows: {}", rows.len());
    println!(
        "header type={:?} id={:?} parentSession={:?}",
        header.get("type"),
        header.get("id"),
        header.get("parentSession")
    );
    let mut types = Vec::<String>::new();
    for row in &rows[1..] {
        let kind = row
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_owned();
        if !types.contains(&kind) {
            types.push(kind);
        }
    }
    println!("entry types: {types:?}");
    let last = rows.last().expect("at least one row");
    println!("last row: {last}");
    Ok(())
}
