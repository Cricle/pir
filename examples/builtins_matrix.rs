//! Real-use matrix for the compiled-in builtins through the SDK.
//!
//! Every section drives a real `Agent` built by [`AgentBuilder`], with real
//! files under a disposable temp root and the scripted offline provider:
//!
//! - `todo`: two prompts creating/updating tasks, then a NEW agent attaching
//!   the session file and listing the replayed state (transcript persistence).
//! - `memory`: remember in one session, recall from an attached session, a
//!   second session directory proving isolation, and `autoRecall` on/off
//!   through `settings.json` (the injected `<memory-context>` block).
//! - `tool-search`: three deferred tools, one `tool_search`, deterministic
//!   result across two fresh builds, and the next request exposing ONLY the
//!   loaded match; a second query unions its match into the active set; a
//!   no-match query activates nothing.
//! - `tool-repair`: a `read` call with an optional `null` field; the repaired
//!   arguments (no `offset`) are what the tool executes and what the
//!   transcript persists.
//! - `subagent`: a real child session inheriting the parent provider; a
//!   successful child answer and a failing child surfaced as tool errors; a
//!   parallel `tasks` dispatch keeps each child's usage and error separate.
//!
//! ```text
//! cargo run --example builtins_matrix --no-default-features \
//!   --features "sdk,session,tools,todo,memory,tool-search,tool-repair,subagent"
//! ```

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure};
use pir::ai::types::{ContentBlock, Message, ThinkingLevel};
use pir::sdk::{AgentBuilder, SdkError, SessionMode};

fn main() -> std::process::ExitCode {
    common::run_big_stack(async {
        if let Err(error) = run().await {
            eprintln!("error: {error:?}");
            return std::process::ExitCode::FAILURE;
        }
        std::process::ExitCode::SUCCESS
    })
}

fn pass(section: &str, detail: &str) {
    println!("PASS [{section}] {detail}");
}

async fn run() -> Result<(), SdkError> {
    let root: PathBuf =
        std::env::temp_dir().join(format!("pir-sdk-builtins-matrix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let agent_dir = root.join("agent");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&project)?;
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::create_dir_all(&sessions)?;
    std::fs::write(
        project.join("notes.txt"),
        "The staging database port is 5432. Written by the SDK matrix example.\n",
    )?;
    // tool-repair calls a real `read`; the file must exist so a repaired call
    // succeeds and a rejected one cannot be confused with an IO error.
    std::fs::write(project.join("x.txt"), "repair probe\n")?;

    todo_matrix(&root, &project, &agent_dir, &sessions).await?;
    memory_matrix(&root, &project, &agent_dir, &sessions).await?;
    tool_search_matrix(&root, &project, &agent_dir).await?;
    tool_repair_matrix(&root, &project, &agent_dir).await?;
    subagent_matrix(&root, &project, &agent_dir).await?;

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

// ---------------------------------------------------------------------------
// todo: multi-turn mutation + transcript replay after a session attach
// ---------------------------------------------------------------------------

async fn todo_matrix(
    root: &Path,
    project: &Path,
    agent_dir: &Path,
    sessions: &Path,
) -> Result<(), SdkError> {
    let _ = root;
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[
            ("todo", json!({"action":"create","subject":"write docs"})),
            ("todo", json!({"action":"create","subject":"run tests"})),
        ]),
        common::text_message("created two tasks"),
        common::tool_calls_message(&[(
            "todo",
            json!({"action":"update","id":1,"status":"in_progress"}),
        )]),
        common::text_message("task 1 in progress"),
        // New agent attaches the same transcript and lists.
        common::tool_calls_message(&[("todo", json!({"action":"list"}))]),
        common::text_message("listed from the replayed session"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline todo matrix agent.")
        .session(SessionMode::Dir(sessions.to_path_buf()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("todo")
        .build()
        .await?;

    assert!(
        agent.tools().iter().any(|tool| tool.name == "todo"),
        "todo builtin must register its tool"
    );
    let created = agent
        .prompt("Create two tasks: write docs, run tests.")
        .await?;
    assert!(created.tool_results.iter().all(|result| !result.is_error));
    let created_text = created.tool_result_texts().join("\n");
    assert!(created_text.contains("write docs"), "{created_text}");
    assert!(created_text.contains("run tests"), "{created_text}");

    let updated = agent.prompt("Start task 1.").await?;
    assert!(updated.tool_results.iter().all(|result| !result.is_error));
    assert!(
        updated
            .tool_result_texts()
            .join("\n")
            .contains("in_progress"),
        "{:?}",
        updated.tool_result_texts()
    );
    let session_path = agent.session_path().expect("persistent todo session");
    agent.shutdown().await?;

    // A brand-new agent (fresh runtime + fresh plugin instance) attaches the
    // JSONL file; the todo plugin must replay the last snapshot from it.
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[("todo", json!({"action":"list"}))]),
        common::text_message("done"),
    ]);
    let resumed = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline todo matrix agent resumed.")
        .session(SessionMode::Dir(sessions.to_path_buf()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("todo")
        .build()
        .await?;
    resumed.attach_session(&session_path).await?;
    let listed = resumed.prompt("List the task board.").await?;
    let listed_text = listed.tool_result_texts().join("\n");
    // PIR-SDK-002: the attached session must replay the transcript through
    // the pure-Rust plugin-host push: tasks 1/2 come back, task 1 in_progress.
    assert!(
        listed_text.contains("write docs")
            && listed_text.contains("run tests")
            && listed_text.contains("in_progress"),
        "attached session must replay the todo transcript (tasks 1/2, task 1 in_progress): {listed_text:?}"
    );
    pass(
        "todo",
        "2 prompts (create x2, update) mutate live state; attach replays tasks 1/2 with task 1 in_progress",
    );
    resumed.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// memory: remember/recall across attach, isolation, autoRecall on/off
// ---------------------------------------------------------------------------

fn write_settings(agent_dir: &Path, auto_recall: bool) -> Result<(), SdkError> {
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_vec_pretty(&json!({
            "memory": { "autoRecall": auto_recall }
        }))
        .unwrap(),
    )?;
    Ok(())
}

async fn memory_matrix(
    root: &Path,
    project: &Path,
    agent_dir: &Path,
    sessions: &Path,
) -> Result<(), SdkError> {
    let _ = root;
    // autoRecall defaults on; keep it explicit for the first run.
    write_settings(agent_dir, true)?;
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[(
            "memory",
            json!({
                "action":"remember",
                "kind":"fact",
                "subject":"staging database port",
                "text":"The staging database port is 5432.",
                "source":"notes.txt"
            }),
        )]),
        common::text_message("remembered"),
    ]);
    let first = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline memory matrix agent.")
        .session(SessionMode::Dir(sessions.to_path_buf()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("memory")
        .build()
        .await?;
    let remembered = first.prompt("Remember the staging database port.").await?;
    assert!(
        remembered
            .tool_results
            .iter()
            .all(|result| !result.is_error),
        "remember failed: {:?}",
        remembered.tool_result_texts()
    );
    let session_path = first.session_path().expect("persistent memory session");
    first.shutdown().await?;

    // Same agent dir + same session directory => same cm store.
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[(
            "memory",
            json!({"action":"recall","query":"staging database port"}),
        )]),
        common::text_message("recall done"),
    ]);
    let second = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline memory matrix agent resumed.")
        .session(SessionMode::Dir(sessions.to_path_buf()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("memory")
        .build()
        .await?;
    second.attach_session(&session_path).await?;
    let recalled = second
        .prompt("What do we know about the staging database port?")
        .await?;
    let recall_text = recalled.tool_result_texts().join("\n");
    assert!(!recalled.tool_results[0].is_error, "{recall_text}");
    assert!(recall_text.contains("5432"), "recall missed: {recall_text}");
    pass(
        "memory",
        "remember then recall from an attached session; file-backed store hit",
    );
    second.shutdown().await?;

    // Isolation: a different session directory means a different cm store.
    let other_sessions = sessions.join("other");
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[(
            "memory",
            json!({"action":"recall","query":"staging database port"}),
        )]),
        common::text_message("recall done"),
    ]);
    let isolated = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline memory isolation agent.")
        .session(SessionMode::Dir(other_sessions))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("memory")
        .build()
        .await?;
    let missed = isolated
        .prompt("What do we know about the staging database port?")
        .await?;
    let missed_text = missed.tool_result_texts().join("\n");
    assert!(
        !missed_text.contains("5432"),
        "a separate session store must not see the memory: {missed_text}"
    );
    pass(
        "memory",
        "separate session directory is isolated (no 5432 leak)",
    );
    isolated.shutdown().await?;

    // autoRecall on: the prompt is transformed with an injected
    // <memory-context> block; off: the prompt is untouched.
    let probe = "Please recall the staging database port fact for the service now";
    let (registry, model, requests) =
        common::ScriptedProvider::registry_recording(vec![common::text_message("answered")]);
    write_settings(agent_dir, true)?;
    let on_agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline memory autoRecall probe.")
        .session(SessionMode::Dir(sessions.to_path_buf()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("memory")
        .build()
        .await?;
    on_agent.prompt(probe).await?;
    let on_users = common::user_texts(&requests.lock().unwrap()[0]).join("\n");
    on_agent.shutdown().await?;

    let (registry, model, requests) =
        common::ScriptedProvider::registry_recording(vec![common::text_message("answered")]);
    write_settings(agent_dir, false)?;
    let off_agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline memory autoRecall probe.")
        .session(SessionMode::Dir(sessions.to_path_buf()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("memory")
        .build()
        .await?;
    off_agent.prompt(probe).await?;
    let off_users = common::user_texts(&requests.lock().unwrap()[0]).join("\n");
    off_agent.shutdown().await?;
    write_settings(agent_dir, true)?;
    assert!(
        off_users.contains(probe) && !off_users.contains("<memory-context"),
        "autoRecall=false must leave the prompt unchanged: {off_users}"
    );
    assert!(
        on_users.contains("<memory-context"),
        "autoRecall=true must inject <memory-context> from the session-adjacent store: {on_users}"
    );
    pass(
        "memory",
        &format!(
            "autoRecall toggle honored (on block={}, off block={}), store follows the session directory",
            on_users.contains("<memory-context"),
            off_users.contains("<memory-context")
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// tool-search: deferred tools, deterministic order, only matches exposed
// ---------------------------------------------------------------------------

struct DeferredProbe {
    name: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
}

impl AgentTool for DeferredProbe {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Probe tool for quantum flux calibration"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }))
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.calls.lock().unwrap().push(format!(
            "{}:{}",
            self.name,
            args["value"].as_str().unwrap_or("")
        ));
        Box::pin(async { ToolResult::ok("probe ran") })
    }
}

async fn run_search_once(
    project: &Path,
    agent_dir: &Path,
    calls: Arc<Mutex<Vec<String>>>,
) -> Result<(String, Vec<String>), SdkError> {
    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::tool_calls_message(&[("tool_search", json!({"query":"quantum flux","limit":1}))]),
        common::text_message("searched"),
        common::tool_calls_message(&[("alpha_quantum_flux", json!({"value":"v1"}))]),
        common::text_message("called the loaded tool"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline tool-search matrix agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(DeferredProbe {
            name: "alpha_quantum_flux",
            calls: Arc::clone(&calls),
        }))
        .with_tool(Arc::new(DeferredProbe {
            name: "beta_quantum_flux",
            calls: Arc::clone(&calls),
        }))
        .with_tool(Arc::new(DeferredProbe {
            name: "gamma_orchid_only",
            calls: Arc::clone(&calls),
        }))
        .builtin("tool-search")
        // `tool_search` itself is model-only + default-inactive (upstream
        // defaultActive:false); a modifier entry turns it on for this agent.
        .tools(["+tool_search"])
        .build()
        .await?;
    assert!(agent.tools().iter().any(|tool| tool.name == "tool_search"));
    let first = agent.prompt("Find the quantum flux tool.").await?;
    let search_text = first.tool_result_texts().join("\n");
    assert!(!first.tool_results[0].is_error, "{search_text}");
    let second = agent.prompt("Use the tool you found.").await?;
    assert!(
        !second.tool_results[0].is_error,
        "{:?}",
        second.tool_result_texts()
    );
    let contexts = requests.lock().unwrap().clone();
    let last_tools: Vec<String> = contexts
        .last()
        .and_then(|context| context.tools.as_ref())
        .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
        .unwrap_or_default();
    agent.shutdown().await?;
    Ok((search_text, last_tools))
}

async fn run_tool_search_union(
    project: &Path,
    agent_dir: &Path,
    calls: Arc<Mutex<Vec<String>>>,
) -> Result<(String, String, Vec<String>), SdkError> {
    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::tool_calls_message(&[("tool_search", json!({"query":"quantum flux","limit":1}))]),
        common::text_message("loaded the quantum tool"),
        common::tool_calls_message(&[("tool_search", json!({"query":"orchid","limit":1}))]),
        common::text_message("loaded the orchid tool"),
        common::tool_calls_message(&[
            ("alpha_quantum_flux", json!({"value":"u1"})),
            ("gamma_orchid_only", json!({"value":"u2"})),
        ]),
        common::text_message("both loaded tools ran"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline tool-search union agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(DeferredProbe {
            name: "alpha_quantum_flux",
            calls: Arc::clone(&calls),
        }))
        .with_tool(Arc::new(DeferredProbe {
            name: "beta_quantum_flux",
            calls: Arc::clone(&calls),
        }))
        .with_tool(Arc::new(DeferredProbe {
            name: "gamma_orchid_only",
            calls: Arc::clone(&calls),
        }))
        .builtin("tool-search")
        .tools(["+tool_search"])
        .build()
        .await?;
    let first = agent.prompt("Load the quantum tool.").await?;
    let first_result = first.tool_result_texts().join("\n");
    assert!(!first.tool_results[0].is_error, "{first_result}");
    let second = agent.prompt("Load the orchid tool as well.").await?;
    let second_result = second.tool_result_texts().join("\n");
    assert!(!second.tool_results[0].is_error, "{second_result}");
    let third = agent.prompt("Call both loaded tools.").await?;
    let third_result = third.tool_result_texts().join("\n");
    assert!(
        third.tool_results.len() == 2 && third.tool_results.iter().all(|result| !result.is_error),
        "both loaded tools must run in one request: {third_result}"
    );
    let contexts = requests.lock().unwrap().clone();
    let tools_at = |index: usize| -> Vec<String> {
        contexts
            .get(index)
            .and_then(|context| context.tools.as_ref())
            .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
            .unwrap_or_default()
    };
    // Request 2 is the first prompt's follow-up (alpha only); request 4 is
    // the second search's follow-up (alpha + gamma union).
    let after_first = tools_at(2);
    let before_call = tools_at(4);
    agent.shutdown().await?;
    Ok((first_result, third_result, {
        assert!(
            after_first.iter().any(|name| name == "alpha_quantum_flux"),
            "{after_first:?}"
        );
        assert!(
            !after_first.iter().any(|name| name == "gamma_orchid_only"),
            "{after_first:?}"
        );
        assert!(
            before_call.iter().any(|name| name == "alpha_quantum_flux"),
            "{before_call:?}"
        );
        assert!(
            before_call.iter().any(|name| name == "gamma_orchid_only"),
            "the second query must union into the active set: {before_call:?}"
        );
        assert!(
            !before_call.iter().any(|name| name == "beta_quantum_flux"),
            "{before_call:?}"
        );
        before_call
    }))
}

async fn run_tool_search_no_match(
    project: &Path,
    agent_dir: &Path,
    calls: Arc<Mutex<Vec<String>>>,
) -> Result<(String, Vec<String>), SdkError> {
    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::tool_calls_message(&[(
            "tool_search",
            json!({"query":"zzz no such capability anywhere"}),
        )]),
        common::text_message("nothing matched"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline tool-search no-match agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .with_tool(Arc::new(DeferredProbe {
            name: "alpha_quantum_flux",
            calls: Arc::clone(&calls),
        }))
        .with_tool(Arc::new(DeferredProbe {
            name: "beta_quantum_flux",
            calls: Arc::clone(&calls),
        }))
        .with_tool(Arc::new(DeferredProbe {
            name: "gamma_orchid_only",
            calls: Arc::clone(&calls),
        }))
        .builtin("tool-search")
        .tools(["+tool_search"])
        .build()
        .await?;
    let searched = agent
        .prompt("Search for an unavailable capability.")
        .await?;
    let search_text = searched.tool_result_texts().join("\n");
    assert!(!searched.tool_results[0].is_error, "{search_text}");
    let contexts = requests.lock().unwrap().clone();
    let last_tools: Vec<String> = contexts
        .last()
        .and_then(|context| context.tools.as_ref())
        .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
        .unwrap_or_default();
    agent.shutdown().await?;
    Ok((search_text, last_tools))
}

async fn tool_search_matrix(root: &Path, project: &Path, agent_dir: &Path) -> Result<(), SdkError> {
    let _ = root;
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let calls_b = Arc::new(Mutex::new(Vec::new()));
    let (search_a, tools_a) = run_search_once(project, agent_dir, Arc::clone(&calls_a)).await?;
    let (search_b, _tools_b) = run_search_once(project, agent_dir, Arc::clone(&calls_b)).await?;
    assert_eq!(
        search_a, search_b,
        "tool_search result must be deterministic"
    );
    assert!(search_a.contains("alpha_quantum_flux"), "{search_a}");
    assert!(
        !search_a.contains("beta_quantum_flux"),
        "limit=1 must load one: {search_a}"
    );
    assert!(!search_a.contains("gamma_orchid_only"), "{search_a}");
    assert!(
        calls_a
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("alpha_quantum_flux:")),
        "the loaded deferred tool must be callable: {:?}",
        calls_a.lock().unwrap()
    );
    assert!(
        tools_a.iter().any(|name| name == "alpha_quantum_flux"),
        "{tools_a:?}"
    );
    assert!(
        !tools_a
            .iter()
            .any(|name| name == "beta_quantum_flux" || name == "gamma_orchid_only"),
        "unloaded deferred tools must stay undeclared: {tools_a:?}"
    );
    println!("note [tool-search] next-request tools: {tools_a:?}");
    pass(
        "tool-search same-run",
        "deferred probe loaded by tool_search in prompt 1 is callable on the next prompt of the SAME AgentSession",
    );

    let union_calls = Arc::new(Mutex::new(Vec::new()));
    let (union_first, union_third, union_tools) =
        run_tool_search_union(project, agent_dir, Arc::clone(&union_calls)).await?;
    assert!(union_first.contains("alpha_quantum_flux"), "{union_first}");
    assert!(union_third.contains("probe ran"), "{union_third}");
    {
        let calls = union_calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("alpha_quantum_flux:"))
                && calls
                    .iter()
                    .any(|call| call.starts_with("gamma_orchid_only:")),
            "both queries' matches must be callable: {calls:?}"
        );
    }
    println!("note [tool-search] union next-request tools: {union_tools:?}");
    pass(
        "tool-search union",
        "query A then query B -> alpha+gamma both active and callable in one request",
    );

    let no_match_calls = Arc::new(Mutex::new(Vec::new()));
    let (no_match_text, no_match_tools) =
        run_tool_search_no_match(project, agent_dir, Arc::clone(&no_match_calls)).await?;
    assert_eq!(
        no_match_text.trim(),
        "No matching tools found.",
        "{no_match_text}"
    );
    assert!(
        !no_match_tools.iter().any(|name| {
            name == "alpha_quantum_flux"
                || name == "beta_quantum_flux"
                || name == "gamma_orchid_only"
        }),
        "a no-match query must activate nothing: {no_match_tools:?}"
    );
    assert!(
        no_match_calls.lock().unwrap().is_empty(),
        "{:?}",
        no_match_calls.lock().unwrap()
    );
    pass(
        "tool-search no-match",
        "empty result activates no deferred tool and the next request stays unchanged",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// tool-repair: malformed optional-null call repaired through the loop
// ---------------------------------------------------------------------------

async fn tool_repair_matrix(root: &Path, project: &Path, agent_dir: &Path) -> Result<(), SdkError> {
    let _ = root;
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[("read", json!({"path":"x.txt","offset":null}))]),
        common::text_message("repair done"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline tool-repair matrix agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .core_tools(true)
        .tools(["read"])
        .builtin("tool-repair")
        .build()
        .await?;
    let outcome = agent.prompt("Read x.txt with an offset field.").await?;
    assert_eq!(outcome.turns, 2, "tool result must be fed back");
    assert!(
        !outcome.tool_results[0].is_error,
        "repaired read call must execute: {:?}",
        outcome.tool_result_texts()
    );
    assert!(
        outcome.tool_result_texts()[0].contains("repair probe"),
        "{:?}",
        outcome.tool_result_texts()
    );
    let transcript = agent.transcript().await;
    let mut repaired = None;
    for message in &transcript {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            if let ContentBlock::ToolCall(call) = block {
                if call.name == "read" {
                    repaired = Some(call.arguments.clone());
                }
            }
        }
    }
    let repaired = repaired.expect("persisted read call");
    assert!(
        !repaired.contains_key("offset"),
        "tool-repair must drop the optional null before execution/persistence: {repaired:?}"
    );
    pass(
        "tool-repair",
        "read {offset:null} repaired by the builtin, executed, and persisted without offset",
    );
    agent.shutdown().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// subagent: real child session, success text + error propagation
// ---------------------------------------------------------------------------

async fn subagent_matrix(root: &Path, project: &Path, agent_dir: &Path) -> Result<(), SdkError> {
    let _ = root;
    let agents_dir = agent_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(
        agents_dir.join("worker.md"),
        "---\nname: worker\ndescription: Offline child worker for the SDK matrix\n---\nAnswer the task exactly as asked.\n",
    )?;

    let mut error_reply = common::text_message("child failed on purpose");
    error_reply.stop_reason = pir::ai::types::StopReason::Error;
    error_reply.error_message = Some("child provider error marker".to_owned());
    let (registry, model, requests) = common::ScriptedProvider::registry_recording(vec![
        common::tool_calls_message(&[(
            "subagent",
            json!({"agent":"worker","task":"Reply with the child marker PIR-CHILD-OK."}),
        )]),
        common::text_message("PIR-CHILD-OK"),
        common::text_message("parent saw the child result"),
        common::tool_calls_message(&[(
            "subagent",
            json!({"agent":"worker","task":"Please fail now."}),
        )]),
        error_reply,
        common::text_message("parent saw the child failure"),
    ]);
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline subagent matrix parent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("subagent")
        .build()
        .await?;
    let ok = agent.prompt("Run the worker.").await?;
    let ok_text = ok.tool_result_texts().join("\n");
    assert!(!ok.tool_results[0].is_error, "{ok_text}");
    assert!(ok_text.contains("PIR-CHILD-OK"), "{ok_text}");
    let contexts = requests.lock().unwrap().clone();
    // The parent's run recorded at least one request whose transcript carried
    // the subagent tool result (the child itself is a separate session; its
    // request is the second one in the shared provider log).
    assert!(
        contexts.len() >= 3,
        "parent + child + parent requests must all reach the provider"
    );
    println!(
        "note [subagent] parent run usage total tokens: {}",
        ok.usage.total_tokens
    );

    let failed = agent
        .prompt("Run the worker again and let it fail.")
        .await?;
    let failed_text = failed.tool_result_texts().join("\n");
    assert!(
        failed.tool_results[0].is_error,
        "child failure must surface as an error tool result: {failed_text}"
    );
    pass(
        "subagent",
        "child inherited parent provider, answered PIR-CHILD-OK, and a failing child surfaced as tool error",
    );
    agent.shutdown().await?;

    // Parallel `tasks`: two children run concurrently against the shared
    // scripted provider. One succeeds, one fails; the aggregate must report
    // 1/2, each result must carry its own (non-doubled) usage, and the error
    // must stay confined to the single failed result.
    let mut parallel_error = common::text_message("PIR-PAR-B failed body");
    parallel_error.stop_reason = pir::ai::types::StopReason::Error;
    parallel_error.error_message = Some("parallel child failure marker".to_owned());
    let (registry, model, _requests) = common::ScriptedProvider::registry_recording(vec![
        common::tool_calls_message(&[(
            "subagent",
            json!({"tasks":[
                {"agent":"worker","task":"Reply with the child marker PIR-PAR-A."},
                {"agent":"worker","task":"Fail with the child marker PIR-PAR-B."}
            ]}),
        )]),
        common::text_message("PIR-PAR-A"),
        parallel_error,
        common::text_message("parent saw the parallel results"),
    ]);
    let parallel_agent = AgentBuilder::new()
        .with_registry(registry, model)
        .agent_dir(agent_dir)
        .cwd(project)
        .system_prompt("Offline subagent parallel parent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .builtin("subagent")
        .build()
        .await?;
    let parallel = parallel_agent
        .prompt("Run both workers in parallel.")
        .await?;
    let parallel_text = parallel.tool_result_texts().join("\n");
    assert!(
        parallel_text.contains("Parallel: 1/2 succeeded"),
        "{parallel_text}"
    );
    assert_eq!(
        parallel_text.matches("completed").count(),
        1,
        "exactly one parallel child completes: {parallel_text}"
    );
    assert_eq!(
        parallel_text.matches("failed (error)").count(),
        1,
        "exactly one parallel child fails: {parallel_text}"
    );
    let details = parallel.tool_results[0]
        .details
        .as_ref()
        .expect("parallel subagent details");
    let results = details["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "{details}");
    for result in results {
        assert_eq!(result["usage"]["input"], 12, "{result}");
        assert_eq!(result["usage"]["output"], 8, "{result}");
        assert_eq!(
            result["usage"]["turns"], 1,
            "per-child usage must not cross: {result}"
        );
    }
    let successes = results
        .iter()
        .filter(|result| result["exitCode"] == 0)
        .count();
    let failures: Vec<&Value> = results
        .iter()
        .filter(|result| result["exitCode"] == 1)
        .collect();
    assert_eq!(successes, 1, "{details}");
    assert_eq!(failures.len(), 1, "{details}");
    assert!(
        failures[0]["errorMessage"]
            .as_str()
            .is_some_and(|message| message.contains("parallel child failure marker")),
        "the failed child's error must stay on its own result: {details}"
    );
    assert!(
        results
            .iter()
            .filter(|result| result["exitCode"] == 0)
            .all(|result| result["stderr"].is_null()),
        "the successful child must not inherit the sibling's error: {details}"
    );
    pass(
        "subagent parallel",
        "tasks[] dispatch: 1/2 succeeded, per-child usage intact, error confined to its own result",
    );
    parallel_agent.shutdown().await?;
    Ok(())
}
