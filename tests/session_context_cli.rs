//! Integration tests for the context-file product surface, the session
//! product layer (`-c/--continue`, `-r/--resume`, `--session`, `--session-id`,
//! `--fork`, `--no-session`), `--mode json`, and the new CLI flags.
//!
//! Patterns follow `tests/agent_session.rs` (scripted provider, hand-rolled
//! `TempDir`); no real network and no `~/.pi` configuration is consulted.
//!
//! The session-layer assertions below encode Windows path resolution
//! (`C:\\proj` stays `C:\\proj`); the platform-neutral session tests live in
//! `tests/suites/session_manager.rs`. Gate the file so Linux CI runs the
//! portable suite instead of re-interpreting these fixtures as relative paths.

#![cfg(windows)]

use std::sync::{Arc, Mutex, OnceLock};

use clap::Parser;

use pir::agent::agent_loop::{run_with_options, AgentEvent, LoopDeps, LoopOptions};
use pir::agent::tool_execution::ToolRegistry;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::cli::Cli;
use pir::coding_agent::context::{
    context_files_enabled, context_system_prompt_with, resolve_context_files_with, tool_infos,
    ContextOptions, MAX_CONTEXT_FILE_BYTES, NO_CONTEXT_FILES_ENV,
};
use pir::coding_agent::json::to_json_event;
use pir::coding_agent::session_resolve::{
    cross_project_session_notice, resolve_session_plan, validate_flags, SessionFlags, SessionPlan,
};
use pir::session_manager::SessionManager;
use pir::system_prompt::ToolInfo;

// ---------------------------------------------------------------------------
// Temp dir helper (same pattern as tests/agent_session.rs)
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-session-context-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn child(&self, relative: &str) -> std::path::PathBuf {
        self.0.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Env serialization (env vars are process-global)
// ---------------------------------------------------------------------------
//
// Only the boundary PARSER test below mutates the process environment; the
// discovery tests drive the explicit `*_with` options instead, so they neither
// read nor write ambient env.

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn set_env(key: &str, value: Option<&str>) {
    match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

// ---------------------------------------------------------------------------
// Context files
// ---------------------------------------------------------------------------

#[test]
fn context_files_global_first_then_ancestors_root_most_first() {
    let root = TempDir::new("ctx-order");
    let global = root.child("agent");
    std::fs::create_dir_all(&global).unwrap();
    let nested = root.child(r"deep\work");
    std::fs::create_dir_all(&nested).unwrap();

    std::fs::write(global.join("AGENTS.md"), "global context").unwrap();
    std::fs::write(root.child("AGENTS.md"), "root context").unwrap();
    std::fs::write(nested.join("CLAUDE.md"), "cwd context").unwrap();

    let files = pir::config::settings::discover_context_files(&nested, &global);
    let contents: Vec<&str> = files.iter().map(|(_, content)| content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["global context", "root context", "cwd context"]
    );
}

#[test]
fn resolve_context_files_respects_agent_dir_and_flag() {
    let root = TempDir::new("ctx-flag");
    let global = root.child("agent");
    std::fs::create_dir_all(&global).unwrap();
    let cwd = root.child("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(global.join("AGENTS.md"), "global context").unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "project context").unwrap();

    // Hermetic: the agent dir is threaded explicitly (no PIR_AGENT_DIR env
    // mutation, no process-global lock).
    let options = ContextOptions {
        disabled: false,
        agent_dir: &global,
        package_dir: None,
    };
    let files = resolve_context_files_with(&cwd, &options);
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].1, "global context");
    assert_eq!(files[1].1, "project context");

    // --no-context-files / PIR_NO_CONTEXT_FILES=1 disables discovery (the env
    // spellings of the flag are covered by the boundary parser test below).
    let disabled = ContextOptions {
        disabled: true,
        ..options
    };
    assert!(resolve_context_files_with(&cwd, &disabled).is_empty());
}

#[test]
fn no_context_files_env_flag_values_parse_at_the_boundary() {
    let _guard = env_lock().lock().unwrap();
    // Unset ⇒ discovery enabled (assertion is hermetic: the value is set
    // explicitly, never inherited).
    set_env(NO_CONTEXT_FILES_ENV, None);
    assert!(context_files_enabled());

    // --no-context-files / PIR_NO_CONTEXT_FILES=1|true disables discovery.
    set_env(NO_CONTEXT_FILES_ENV, Some("1"));
    assert!(!context_files_enabled());
    set_env(NO_CONTEXT_FILES_ENV, Some("true"));
    assert!(!context_files_enabled());

    // Restore: unset ⇒ enabled again.
    set_env(NO_CONTEXT_FILES_ENV, None);
    assert!(context_files_enabled());
}

#[test]
fn context_files_over_size_cap_are_skipped() {
    let root = TempDir::new("ctx-cap");
    let global = root.child("agent");
    std::fs::create_dir_all(&global).unwrap();
    let cwd = root.child("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let oversized = "x".repeat((MAX_CONTEXT_FILE_BYTES + 1) as usize);
    std::fs::write(cwd.join("AGENTS.md"), &oversized).unwrap();
    std::fs::write(global.join("AGENTS.md"), "small global").unwrap();

    let files = pir::config::settings::discover_context_files(&cwd, &global);
    assert_eq!(files.len(), 1, "oversized file must be skipped");
    assert_eq!(files[0].1, "small global");
}

#[test]
fn context_system_prompt_injects_project_context_section() {
    let root = TempDir::new("ctx-prompt");
    let global = root.child("agent");
    std::fs::create_dir_all(&global).unwrap();
    let cwd = root.child("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "always use tabs").unwrap();

    // Hermetic: agent dir and disable flag come in as explicit options (no
    // ambient PIR_AGENT_DIR / PIR_NO_CONTEXT_FILES reads).
    let options = ContextOptions {
        disabled: false,
        agent_dir: &global,
        package_dir: None,
    };
    let tools: Vec<ToolInfo> = Vec::new();
    let prompt =
        context_system_prompt_with(&cwd, &tools, &options).expect("prompt with context files");
    assert!(prompt.contains("<project_context>"));
    assert!(prompt.contains("<project_instructions path=\""));
    assert!(prompt.contains("always use tabs"));

    // Flag off → the context FILES are not injected. `--no-context-files`
    // (upstream: "Disable AGENTS.md and CLAUDE.md discovery and loading")
    // governs files only — skills discovery is deliberately NOT gated (see
    // `ContextOptions::disabled`'s doc) and the user-global `~/.agents/skills`
    // root comes from the real home, so this environment is not hermetic for
    // "the whole prompt is None": that only holds when the machine has no user
    // skills. Pin the flag's own contract those two ways.
    let disabled = ContextOptions {
        disabled: true,
        ..options
    };
    assert!(
        pir::coding_agent::context::resolve_context_files_with(&cwd, &disabled).is_empty(),
        "the disable flag must suppress context-file discovery"
    );
    if let Some(prompt) = context_system_prompt_with(&cwd, &tools, &disabled) {
        assert!(!prompt.contains("<project_context>"), "{prompt}");
        assert!(!prompt.contains("<project_instructions"), "{prompt}");
        assert!(!prompt.contains("always use tabs"), "{prompt}");
        // The prompt stays `Some` here because the skills LIST comes from the
        // real home (see above); its rendered `<available_skills>` section
        // additionally needs a declared `read`/`bash` tool, which this fixture
        // deliberately does not pass — so the section is absent either way.
        assert!(!prompt.contains("<available_skills>"), "{prompt}");
    }

    // No context files (missing agent dir, bare cwd) → nothing from files; the
    // same ambient-skills caveat applies to the whole-prompt check.
    let bare = root.child("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let missing_agent = root.child("missing-agent");
    let no_files = ContextOptions {
        agent_dir: &missing_agent,
        ..options
    };
    assert!(pir::coding_agent::context::resolve_context_files_with(&bare, &no_files).is_empty());
    if let Some(prompt) = context_system_prompt_with(&bare, &tools, &no_files) {
        assert!(!prompt.contains("<project_instructions"), "{prompt}");
    }
}

#[test]
fn tool_infos_extracts_registry_entries() {
    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let infos = tool_infos(&registry);
    assert!(infos.iter().any(|tool| tool.name == "read"));
    assert!(infos.iter().all(|tool| !tool.description.is_empty()));
}

// ---------------------------------------------------------------------------
// Session listing / continue resolution
// ---------------------------------------------------------------------------

fn temp_session_dir(label: &str) -> TempDir {
    TempDir::new(label)
}

fn create_session(dir: &std::path::Path, cwd: &str) -> String {
    // The upstream file-creation rule: the file materializes on the first
    // conversation message, so seed one to make the session discoverable.
    let mut session = SessionManager::create(cwd, dir).unwrap();
    let id = session.id().to_owned();
    session
        .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
            role: pir::ai::types::UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: "seed".into(),
                    text_signature: None,
                },
            )],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .unwrap();
    id
}

#[test]
fn list_sessions_sorts_by_mtime_descending_and_filters_cwd() {
    let dir = temp_session_dir("list");
    let first = create_session(dir.path(), "C:\\proj");
    std::thread::sleep(std::time::Duration::from_millis(120));
    let second = create_session(dir.path(), "C:\\proj");
    std::thread::sleep(std::time::Duration::from_millis(120));
    let other_cwd = create_session(dir.path(), "C:\\other");

    let all = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, other_cwd, "most recently modified first");
    assert_eq!(all[1].id, second);
    assert_eq!(all[2].id, first);

    let filtered = SessionManager::list_sessions(dir.path(), Some("C:\\proj"));
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].id, second);
    assert_eq!(filtered[1].id, first);
}

#[test]
fn find_session_by_id_matches_exact_header_id() {
    let dir = temp_session_dir("find");
    let id = create_session(dir.path(), "C:\\proj");
    let found = SessionManager::find_session_by_id(dir.path(), &id)
        .expect("exact header id resolves to a file");
    assert!(
        found
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&format!("_{id}.jsonl")),
        "the file name carries the upstream <timestamp>_<id>.jsonl shape: {}",
        found.display()
    );
    assert!(SessionManager::find_session_by_id(dir.path(), "missing-id").is_none());
}

#[test]
fn create_with_id_uses_exact_id_and_rejects_duplicates() {
    let dir = temp_session_dir("create-id");
    let manager = SessionManager::create_with_id("C:\\proj", dir.path(), "fixed-id-123").unwrap();
    assert_eq!(manager.id(), "fixed-id-123");
    // Upstream file name: <timestamp>_<session id>.jsonl.
    assert!(manager
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with("_fixed-id-123.jsonl"));
    assert_eq!(manager.cwd(), "C:\\proj");

    // Duplicate ids collide on the same (planned) file: the second create
    // must fail when the file already exists (after a conversation) or at
    // least yield the same path.
    assert!(SessionManager::create_with_id("C:\\proj", dir.path(), "bad id").is_err());
    assert!(SessionManager::create_with_id("C:\\proj", dir.path(), "a/b").is_err());
    assert!(SessionManager::create_with_id("C:\\proj", dir.path(), "a\\b").is_err());
}

#[test]
fn fork_from_source_copies_history_into_target_with_target_cwd() {
    let source_dir = temp_session_dir("fork-src");
    let mut source = SessionManager::create("C:\\origin", source_dir.path()).unwrap();
    source
        .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
            role: pir::ai::types::UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: "hello from origin".into(),
                    text_signature: None,
                },
            )],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .unwrap();
    let source_id = source.id().to_owned();
    let _ = source_id;

    let target_dir = temp_session_dir("fork-dst");
    let fork =
        SessionManager::fork_from_source(source.path(), "C:\\target", target_dir.path(), None)
            .unwrap();
    assert_ne!(fork.id(), source_id);
    assert_eq!(
        fork.cwd(),
        "C:\\target",
        "fork header carries the target cwd"
    );
    assert_eq!(
        fork.parent_session(),
        Some(source.path().to_string_lossy().as_ref()),
        "fork header points at the source session file"
    );
    assert_eq!(
        fork.path().parent().unwrap(),
        target_dir.path(),
        "fork lands in the target session dir"
    );
    let texts = fork
        .build_context_entries()
        .unwrap()
        .iter()
        .filter_map(|message| match message {
            pir::ai::types::Message::User(user) => match &user.content[0] {
                pir::ai::types::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"hello from origin".to_owned()));

    // continue_recent in the target project now finds the fork.
    let continued = SessionManager::continue_recent(target_dir.path(), "C:\\target").unwrap();
    assert_eq!(continued.id(), fork.id());
}

// ---------------------------------------------------------------------------
// Session flag resolution (upstream main.ts createSessionManager semantics)
// ---------------------------------------------------------------------------

fn flags(
    no_session: bool,
    continue_recent: bool,
    resume: bool,
    session: Option<&str>,
    session_id: Option<&str>,
    fork: Option<&str>,
) -> SessionFlags {
    SessionFlags {
        no_session,
        continue_recent,
        resume,
        session: session.map(str::to_owned),
        session_id: session_id.map(str::to_owned),
        fork: fork.map(str::to_owned),
    }
}

#[test]
fn flag_conflicts_match_upstream() {
    // Upstream validates --fork and --session-id conflicts only; -c + -r is
    // resolved by precedence (resume wins), not rejected.
    assert!(validate_flags(&flags(false, true, false, None, None, None)).is_ok());
    let error = validate_flags(&flags(false, true, false, None, None, Some("abc")))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("--fork cannot be combined with --continue"),
        "{error}"
    );
    let error = validate_flags(&flags(false, false, false, Some("x"), Some("y"), None))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("--session-id cannot be combined with --session"),
        "{error}"
    );
    // --fork + --no-session conflict, --session-id + --no-session is allowed
    // upstream (noSession wins).
    assert!(validate_flags(&flags(true, false, false, None, None, Some("abc"))).is_err());
    assert!(validate_flags(&flags(true, false, false, None, Some("y"), None)).is_ok());
    assert!(validate_flags(&flags(false, false, false, None, None, None)).is_ok());
}

#[test]
fn resolution_precedence_and_errors_match_upstream() {
    let dir = temp_session_dir("resolve");
    let cwd = dir.child("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let first = create_session(dir.path(), cwd.to_string_lossy().as_ref());
    std::thread::sleep(std::time::Duration::from_millis(120));
    let second = create_session(dir.path(), cwd.to_string_lossy().as_ref());
    std::thread::sleep(std::time::Duration::from_millis(120));
    let other = create_session(dir.path(), "C:\\elsewhere");

    // --no-session wins over everything (and -c + -r resolves to the
    // no-session plan).
    assert_eq!(
        resolve_session_plan(
            &flags(true, true, true, None, None, None),
            &cwd,
            dir.path(),
            false
        )
        .unwrap(),
        SessionPlan::Ephemeral { id: None }
    );

    // --continue picks the most recent session for the cwd.
    match resolve_session_plan(
        &flags(false, true, false, None, None, None),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Open(path) => assert!(path
            .to_string_lossy()
            .ends_with(&format!("_{second}.jsonl"))),
        other => panic!("expected Open, got {other:?}"),
    }

    // --continue with no sessions for the cwd creates a new one.
    let empty = temp_session_dir("resolve-empty");
    assert_eq!(
        resolve_session_plan(
            &flags(false, true, false, None, None, None),
            &cwd,
            empty.path(),
            false
        )
        .unwrap(),
        SessionPlan::New { id: None }
    );

    // --session exact id and prefix.
    match resolve_session_plan(
        &flags(false, false, false, Some(&first), None, None),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Open(path) => {
            assert!(path.to_string_lossy().ends_with(&format!("_{first}.jsonl")))
        }
        other => panic!("expected Open, got {other:?}"),
    }
    let prefix: String = second.chars().take(24).collect();
    match resolve_session_plan(
        &flags(false, false, false, Some(&prefix), None, None),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Open(path) => assert!(path
            .to_string_lossy()
            .ends_with(&format!("_{second}.jsonl"))),
        other => panic!("expected Open, got {other:?}"),
    }

    // --session as a literal path resolves relative to cwd.
    let session_file = cwd.join("direct.jsonl");
    SessionManager::create(cwd.to_string_lossy().as_ref(), dir.path()).unwrap();
    std::fs::copy(
        SessionManager::find_session_by_id(dir.path(), &second).unwrap(),
        &session_file,
    )
    .unwrap();
    assert!(matches!(
        resolve_session_plan(
            &flags(false, false, false, Some("direct.jsonl"), None, None),
            &cwd,
            dir.path(),
            false
        )
        .unwrap(),
        SessionPlan::Open(_)
    ));

    // --session-id: existing opens, missing creates with that id.
    match resolve_session_plan(
        &flags(false, false, false, None, Some(&first), None),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Open(path) => {
            assert!(path.to_string_lossy().ends_with(&format!("_{first}.jsonl")))
        }
        other => panic!("expected Open, got {other:?}"),
    }
    assert_eq!(
        resolve_session_plan(
            &flags(false, false, false, None, Some("brand-new-id"), None),
            &cwd,
            dir.path(),
            false
        )
        .unwrap(),
        SessionPlan::New {
            id: Some("brand-new-id".to_owned())
        }
    );

    // --fork: new fork plan from the most recent matching session.
    match resolve_session_plan(
        &flags(false, false, false, None, None, Some(&other)),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Fork { source, id } => {
            assert!(source
                .to_string_lossy()
                .ends_with(&format!("_{other}.jsonl")));
            assert_eq!(id, None);
        }
        other => panic!("expected Fork, got {other:?}"),
    }

    // --fork with an unknown id errors like upstream.
    let error = resolve_session_plan(
        &flags(false, false, false, None, None, Some("nope")),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("No session found matching 'nope'"),
        "{error}"
    );

    // --fork + --session-id when that id already exists errors.
    let error = resolve_session_plan(
        &flags(false, false, false, None, Some(&first), Some(&other)),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(&format!("Session already exists with id '{first}'")),
        "{error}"
    );

    // --resume: interactive → Pick, headless → this project's most recent
    // session (upstream mergeSession plan always has a session; without a
    // picker the picker's default row is the project's newest session).
    assert_eq!(
        resolve_session_plan(
            &flags(false, false, true, None, None, None),
            &cwd,
            dir.path(),
            true
        )
        .unwrap(),
        SessionPlan::Pick
    );
    let newest = create_session(dir.path(), cwd.to_string_lossy().as_ref());
    match resolve_session_plan(
        &flags(false, false, true, None, None, None),
        &cwd,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Open(path) => assert!(path
            .to_string_lossy()
            .ends_with(&format!("_{newest}.jsonl"))),
        other => panic!("expected Open (headless resume), got {other:?}"),
    }
}

#[test]
fn headless_resume_picks_the_newest_current_project_session_or_starts_new() {
    let dir = temp_session_dir("headless-resume");
    let cwd = std::path::PathBuf::from("C:\\proj-resume");
    let resume = SessionFlags {
        resume: true,
        ..SessionFlags::default()
    };

    // Empty project: headless resume falls back to a new session (the
    // interactive host's empty-picker fallback).
    assert_eq!(
        resolve_session_plan(&resume, &cwd, dir.path(), false).unwrap(),
        SessionPlan::New { id: None }
    );

    // Two sessions of this project plus a NEWER session of another project:
    // resume stays scoped to the current project (the picker's default
    // "current" tab) and takes its most recent entry.
    let older = create_session(dir.path(), cwd.to_string_lossy().as_ref());
    std::thread::sleep(std::time::Duration::from_millis(120));
    let newer = create_session(dir.path(), cwd.to_string_lossy().as_ref());
    std::thread::sleep(std::time::Duration::from_millis(120));
    let _other_project = create_session(dir.path(), "C:\\elsewhere");

    match resolve_session_plan(&resume, &cwd, dir.path(), false).unwrap() {
        SessionPlan::Open(path) => {
            let file = path.to_string_lossy().into_owned();
            assert!(file.ends_with(&format!("_{newer}.jsonl")), "{file}");
            assert!(!file.ends_with(&format!("_{older}.jsonl")), "{file}");
        }
        other => panic!("expected Open (headless resume), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P2-12: a cross-project --session is FORKED into the current project
// (upstream warns "Session found in different project" and offers exactly
// that fork).
// ---------------------------------------------------------------------------

#[test]
fn cross_project_session_ids_fork_into_the_current_project() {
    let dir = temp_session_dir("cross-project");
    let cwd_a = std::path::PathBuf::from("C:\\project-a");
    let cwd_b = "C:\\project-b";

    let in_project = create_session(dir.path(), cwd_a.to_string_lossy().as_ref());
    let other_project = create_session(dir.path(), cwd_b);

    // A same-project id still opens.
    match resolve_session_plan(
        &flags(false, false, false, Some(&in_project), None, None),
        &cwd_a,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Open(path) => assert!(path
            .to_string_lossy()
            .ends_with(&format!("_{in_project}.jsonl"))),
        other => panic!("expected Open, got {other:?}"),
    }

    // A cross-project exact id is COPIED into the current project (upstream
    // `resolveSessionPath` type "global" -> `forkSessionOrExit`): the plan is
    // a Fork whose new session header records cwd_a, never an in-place Open
    // of cwd_b's session.
    let notice = cross_project_session_notice(cwd_b);
    assert_eq!(notice, "Session found in different project: C:\\project-b");
    match resolve_session_plan(
        &flags(false, false, false, Some(&other_project), None, None),
        &cwd_a,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Fork { source, id } => {
            assert!(source
                .to_string_lossy()
                .ends_with(&format!("_{other_project}.jsonl")));
            assert_eq!(id, None);
        }
        other => panic!("expected Fork (cross-project copy), got {other:?}"),
    }

    // A cross-project id PREFIX resolves the same way.
    let prefix: String = other_project.chars().take(24).collect();
    assert!(matches!(
        resolve_session_plan(
            &flags(false, false, false, Some(&prefix), None, None),
            &cwd_a,
            dir.path(),
            false,
        )
        .unwrap(),
        SessionPlan::Fork { .. }
    ));

    // Forking a cross-project session stays allowed (it forks into the
    // current directory; upstream `forkSessionOrExit` without prompting).
    match resolve_session_plan(
        &flags(false, false, false, None, None, Some(&other_project)),
        &cwd_a,
        dir.path(),
        false,
    )
    .unwrap()
    {
        SessionPlan::Fork { source, id } => {
            assert!(source
                .to_string_lossy()
                .ends_with(&format!("_{other_project}.jsonl")));
            assert_eq!(id, None);
        }
        other => panic!("expected Fork, got {other:?}"),
    }

    // The genuinely unresolvable ref keeps the upstream error.
    let error = resolve_session_plan(
        &flags(false, false, false, Some("does-not-exist"), None, None),
        &cwd_a,
        dir.path(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("No session found matching 'does-not-exist'"),
        "{error}"
    );

    // --no-session + --session-id carries the id on the ephemeral plan.
    assert_eq!(
        resolve_session_plan(
            &flags(true, false, false, None, Some("explicit-id"), None),
            &cwd_a,
            dir.path(),
            false
        )
        .unwrap(),
        SessionPlan::Ephemeral {
            id: Some("explicit-id".to_owned())
        }
    );
}

#[test]
fn open_session_manager_materializes_plans() {
    let dir = temp_session_dir("materialize");
    let cwd_owned = std::env::temp_dir().join("pir-session-context-proj");
    let cwd = cwd_owned.to_str().expect("temp dir path is valid UTF-8");

    // Ephemeral → None.
    assert!(
        open_session_manager_helper(&SessionPlan::Ephemeral { id: None }, cwd, dir.path())
            .unwrap()
            .is_none()
    );

    // New → file with header cwd.
    let manager = open_session_manager_helper(&SessionPlan::New { id: None }, cwd, dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(manager.cwd(), cwd);

    // New with id → exact id.
    let manager = open_session_manager_helper(
        &SessionPlan::New {
            id: Some("plan-id-1".into()),
        },
        cwd,
        dir.path(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(manager.id(), "plan-id-1");

    // Open → reopens the same file.
    let path = manager.path().to_path_buf();
    let reopened = open_session_manager_helper(&SessionPlan::Open(path.clone()), cwd, dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(reopened.path(), path);
}

fn open_session_manager_helper(
    plan: &SessionPlan,
    cwd: &str,
    session_dir: &std::path::Path,
) -> pir::Result<Option<SessionManager>> {
    // Thin wrapper so the private-module helper is exercised through the
    // public API surface used by the print host.
    pir::coding_agent::session_resolve::resolve_session_plan(
        &SessionFlags {
            no_session: matches!(plan, SessionPlan::Ephemeral { .. }),
            ..SessionFlags::default()
        },
        std::path::Path::new(cwd),
        session_dir,
        false,
    )?;
    match plan {
        SessionPlan::Ephemeral { .. } => Ok(None),
        SessionPlan::New { id: Some(id) } => {
            Ok(Some(SessionManager::create_with_id(cwd, session_dir, id)?))
        }
        SessionPlan::New { id: None } => Ok(Some(SessionManager::create(cwd, session_dir)?)),
        SessionPlan::Open(path) => Ok(Some(SessionManager::open(path)?)),
        SessionPlan::Fork { source, id } => Ok(Some(SessionManager::fork_from_source(
            source,
            cwd,
            session_dir,
            id.as_deref(),
        )?)),
        SessionPlan::Pick => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// --mode json event shape
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
        context_window: 4096,
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: std::collections::BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Provider that emits a full stream lifecycle (start, toolcall_start with a
/// partial snapshot, toolcall_end, done) so the JSON transform's
/// partial-stripping is exercised.
struct StreamingProvider;

impl Provider for StreamingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        let mut partial = assistant(vec![tool_call("call-1", "read")], StopReason::ToolUse);
        partial.usage.total_tokens = 42;
        stream.push(AssistantMessageEvent::Start {
            partial: Arc::new(partial.clone()),
        });
        stream.push(AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            partial: Arc::new(partial.clone()),
        });
        stream.push(AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: std::collections::BTreeMap::new(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            },
            partial: Arc::new(partial),
        });
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant(vec![text("done")], StopReason::Stop),
        ));
        stream
    }
}

#[test]
fn json_event_transform_strips_partial_and_hoists_usage() {
    // message_update from a toolcall_start carries id/toolName, no partial.
    let mut partial = assistant(vec![tool_call("call-1", "read")], StopReason::ToolUse);
    partial.usage.total_tokens = 42;
    let event = AgentEvent::StreamEvent {
        turn: 0,
        event: AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            partial: Arc::new(partial.clone()),
        },
    };
    let value = to_json_event(&event);
    assert_eq!(value["type"], "message_update");
    assert_eq!(value["usage"]["totalTokens"], 42);
    let assistant_event = &value["assistantMessageEvent"];
    assert_eq!(assistant_event["type"], "toolcall_start");
    assert_eq!(assistant_event["id"], "call-1");
    assert_eq!(assistant_event["toolName"], "read");
    assert!(assistant_event.get("partial").is_none());

    // Non-stream events pass through unchanged (RPC serialization shape).
    let value = to_json_event(&AgentEvent::AgentStart {
        context: Arc::new(Context::default()),
    });
    assert_eq!(value["type"], "agent_start");
}

#[tokio::test]
async fn json_mode_emits_parseable_event_lines_without_partials() {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(StreamingProvider));
    let lines: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let events: pir::agent_session::EventHandler = Arc::new(move |event: AgentEvent| {
        sink.lock().unwrap().push(to_json_event(&event));
    });

    let mut tools = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut tools);
    let deps = LoopDeps {
        tools: Arc::new(tools),
        ..LoopDeps::default()
    }
    .with_provider_unchecked(Arc::new(registry), fake_model(), StreamOptions::default());
    let registry_for_stream = {
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(StreamingProvider));
        Arc::new(registry)
    };
    // The loop forwards per-event stream updates only through the
    // `event_stream` override (same pattern as tests/agent_session.rs).
    let event_stream: pir::agent::agent_loop::EventStreamFn = {
        let registry = registry_for_stream;
        Box::new(move |request: pir::agent::agent_loop::ProviderRequest| {
            let stream = registry.stream_unchecked(
                request.model.unwrap_or_else(fake_model),
                // `ProviderRequest.context` is shared (`Arc<Context>`); the
                // provider trait takes an owned context.
                pir::agent::agent_loop::context_into_owned(request.context),
                StreamOptions::default(),
            );
            Box::pin(async move { stream })
        })
    };
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("hi")],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        tools: None,
    };
    run_with_options(
        deps,
        context,
        None,
        LoopOptions {
            events: Some(events),
            event_stream: Some(event_stream),
            ..LoopOptions::default()
        },
    )
    .await;

    let lines = lines.lock().unwrap();
    let types: Vec<String> = lines
        .iter()
        .filter_map(|value| value["type"].as_str().map(str::to_owned))
        .collect();
    assert!(types.contains(&"agent_start".to_owned()));
    assert!(types.contains(&"agent_end".to_owned()));
    assert!(types.contains(&"message_update".to_owned()));
    for value in lines.iter() {
        let serialized = serde_json::to_string(value).unwrap();
        assert!(
            !serialized.contains("\"partial\""),
            "leaked partial: {serialized}"
        );
    }
    // Every line must be standalone-parseable JSON.
    for value in lines.iter() {
        let serialized = serde_json::to_string(value).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, *value);
    }
}

#[test]
fn session_header_serializes_as_first_json_line() {
    let dir = temp_session_dir("json-header");
    let cwd_owned = std::env::temp_dir().join("pir-session-context-proj");
    let cwd = cwd_owned.to_str().expect("temp dir path is valid UTF-8");
    let session = SessionManager::create(cwd, dir.path()).unwrap();
    let header = session
        .header()
        .expect("persisting sessions carry a header");
    // The json-mode first line serializes the on-disk header shape.
    let value =
        serde_json::to_value(pir::session_manager::FileEntry::Session(header.clone())).unwrap();
    assert_eq!(value["type"], "session");
    assert_eq!(
        value["version"],
        pir::session_manager::CURRENT_SESSION_VERSION
    );
    assert_eq!(value["cwd"], cwd);
    assert!(value["id"].is_string());
}

// ---------------------------------------------------------------------------
// CLI flag parsing
// ---------------------------------------------------------------------------

#[test]
fn cli_parses_new_session_and_mode_flags() {
    let cli = Cli::parse_from([
        "pir",
        "print",
        "hello",
        "--mode",
        "json",
        "-c",
        "--no-context-files",
    ]);
    assert_eq!(cli.mode(), Some(pir::coding_agent::cli::Mode::Json));
    assert!(cli.continue_session);
    assert!(cli.no_context_files);
    let flags = cli.session_flags();
    assert!(flags.continue_recent);
    assert!(!flags.resume);

    let cli = Cli::parse_from([
        "pir",
        "--session",
        "abc123",
        "--session-id",
        "xyz",
        "interactive",
    ]);
    let flags = cli.session_flags();
    assert_eq!(flags.session.as_deref(), Some("abc123"));
    assert_eq!(flags.session_id.as_deref(), Some("xyz"));
    assert_eq!(cli.mode(), None);

    let cli = Cli::parse_from(["pir", "--fork", "deadbeef", "--no-session", "interactive"]);
    let flags = cli.session_flags();
    assert_eq!(flags.fork.as_deref(), Some("deadbeef"));
    assert!(flags.no_session);
    assert!(
        validate_flags(&flags).is_err(),
        "--fork + --no-session conflict"
    );
}

#[test]
fn cli_rejects_conflicting_session_flags() {
    let cli = Cli::parse_from(["pir", "-c", "-r", "interactive"]);
    let flags = cli.session_flags();
    // Upstream resolves -c + -r by precedence (resume wins); assert that.
    assert!(flags.resume && flags.continue_recent);
    assert!(validate_flags(&flags).is_ok());
}

#[test]
fn cli_mode_validation_hard_errors_on_unknown_values() {
    assert_eq!(
        pir::coding_agent::cli::normalize_mode(Some("json")).unwrap(),
        Some(pir::coding_agent::cli::Mode::Json)
    );
    assert_eq!(
        pir::coding_agent::cli::normalize_mode(Some("rpc")).unwrap(),
        Some(pir::coding_agent::cli::Mode::Rpc)
    );
    assert_eq!(
        pir::coding_agent::cli::normalize_mode(Some("text")).unwrap(),
        Some(pir::coding_agent::cli::Mode::Text)
    );
    // Invalid values are hard errors (upstream exits 1), never fallbacks.
    let error = pir::coding_agent::cli::normalize_mode(Some("bogus"))
        .expect_err("invalid mode must be an error");
    assert!(error.to_string().contains("Invalid mode"));
    assert_eq!(pir::coding_agent::cli::normalize_mode(None).unwrap(), None);
    // The argv-level gate rejects invalid values in both spellings.
    assert!(
        pir::coding_agent::cli::validate_mode_arg(&["--mode".to_owned(), "json".to_owned()])
            .is_ok()
    );
    assert!(pir::coding_agent::cli::validate_mode_arg(&["--mode=bogus".to_owned()]).is_err());
    assert!(
        pir::coding_agent::cli::validate_mode_arg(&["--mode".to_owned(), "bogus".to_owned()])
            .is_err()
    );
    assert!(pir::coding_agent::cli::validate_mode_arg(&["--mode".to_owned()]).is_err());
    // Values after `--` are positional, not flags.
    assert!(pir::coding_agent::cli::validate_mode_arg(&[
        "--".to_owned(),
        "--mode".to_owned(),
        "bogus".to_owned()
    ])
    .is_ok());
}

#[test]
fn cli_no_context_files_flag_is_long_only_like_upstream_name() {
    let cli = Cli::parse_from(["pir", "interactive", "--no-context-files"]);
    assert!(cli.no_context_files);
}

// ---------------------------------------------------------------------------
// `session` off: a session reference must report the real cause. The enabled
// build keeps the upstream wording byte-for-byte.
// ---------------------------------------------------------------------------

#[test]
fn session_not_found_message_matches_the_build_configuration() {
    let message = pir::coding_agent::session_resolve::session_not_found_message("abc");
    #[cfg(feature = "session")]
    assert_eq!(message, "No session found matching 'abc'");
    #[cfg(not(feature = "session"))]
    assert_eq!(message, pir::session_manager::SESSION_PERSISTENCE_DISABLED);
}

#[test]
fn new_session_id_warning_matches_the_build_configuration() {
    let message = pir::coding_agent::session_resolve::new_session_id_warning("abc");
    #[cfg(feature = "session")]
    assert_eq!(
        message,
        "Warning: No project session found with id 'abc'; creating a new session with that id."
    );
    #[cfg(not(feature = "session"))]
    {
        assert!(
            message.contains("session persistence is disabled"),
            "{message}"
        );
        assert!(message.contains("in memory only"), "{message}");
        assert!(!message.contains("creating a new session"), "{message}");
        assert!(message.contains("'abc'"), "{message}");
    }
}

#[cfg(not(feature = "session"))]
#[test]
fn session_and_fork_refs_report_persistence_disabled() {
    let dir = temp_session_dir("session-off-refs");
    let cwd = std::path::Path::new("C:\nowhere");
    for flags in [
        flags(false, false, false, Some("missing-id"), None, None),
        flags(false, false, false, None, None, Some("missing-id")),
    ] {
        let error = resolve_session_plan(&flags, cwd, dir.path(), false)
            .expect_err("no session can resolve without persistence")
            .to_string();
        assert_eq!(error, pir::session_manager::SESSION_PERSISTENCE_DISABLED);
    }
}
