//! Executes installed pi extension packages through the REAL QuickJS runtime.
//!
//! `tests/ecosystem_compat.rs` proves the loader can *ingest* every installed
//! package (discovery, transpile, dependency scan) without evaluating it;
//! `tests/pi_shims.rs` proves a handful of real entries *load*. This harness
//! closes the remaining gap: it starts a real [`ExtensionHandle`] actor
//! (QuickJS + prelude + host functions), loads the installed package through
//! its `pi.extensions` manifest, and then **drives it**:
//!
//! 1. `load` reports one loaded extension and zero error entries;
//! 2. the factory registered its documented artifacts (tools / commands /
//!    flags / shortcuts / hooks), inspected through the public registry
//!    accessors;
//! 3. a representative event dispatch and/or tool call runs through QuickJS
//!    and returns a structured result with no handler errors, no panic and no
//!    timeout.
//!
//! The installed npm tree is user-owned and read-only here: packages are
//! discovered from `<agent-dir>/npm/node_modules` and pointed at in place.
//! The runtime itself is redirected to a per-run temp agent dir
//! (`PIR_AGENT_DIR`) and a temp `XDG_CONFIG_HOME` (the rpiv-* config root), so
//! neither the engine nor the extensions read or write the operator's
//! `~/.pi`/`~/.config` state. Those environment writes happen once, before any
//! runtime starts; integration tests are a dedicated process, so no other test
//! binary observes them.
//!
//! Every wait is bounded: actor start and load under an explicit
//! `tokio::time` budget, event/tool dispatches under the injected
//! [`ExtensionLimits`] call timeout (the same knob the host uses), and the
//! `before_agent_start` dispatch under its shorter lifecycle budget.
//!
//! Packages covered (small, no network, no long-lived servers):
//! - `@juicesharp/rpiv-todo` — `todo` tool, `/todos` command, lifecycle hooks;
//! - `pi-tool-repair` — `before_provider_request` + `message_end` hooks;
//! - `@juicesharp/rpiv-i18n` — `--locale` flag, `/languages` command,
//!   `session_start` hook;
//! - `@vndv/pi-codegraph` — five `codegraph_*` tools + `before_agent_start`
//!   prompt hook (its tools are *not* executed: they would spawn the
//!   `codegraph` binary).
//!
//! Deliberately excluded (documented startup/runtime hazards, not silently
//! skipped here because they are not part of this matrix): `pi-phone`
//! (binds a server/port), `pi-web-access` (network), `context-mode` (embedded
//! SQLite + large startup), `pi-subagents` (spawns child sessions). See
//! `logs/perf/ecosystem-runtime-<date>.md`.
//!
//! Run with:
//! `cargo test --test ecosystem_runtime_exec -- --nocapture`
//! Set `PIR_REQUIRE_INSTALLED_EXTENSIONS=1` to turn a missing package from a
//! reported skip into a failure.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::json;

use pir::extensions::{
    ExtensionConfig, ExtensionHandle, ExtensionLimits, FlagValue, LoadResult, Notification,
    SessionContextSnapshot,
};

/// Actor construction includes the QuickJS runtime plus the injected prelude.
const START_BUDGET: Duration = Duration::from_secs(60);
/// Package load includes discovery, transpile, module evaluation and (for
/// rpiv-todo) a top-level dynamic import of the i18n SDK.
const LOAD_BUDGET: Duration = Duration::from_secs(60);
/// Per-dispatch budget handed to the runtime itself (not just the caller).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The shorter lifecycle budget `before_agent_start` runs under.
const BEFORE_AGENT_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Temp roots shared by every test in this binary. `PIR_AGENT_DIR` is
/// process-global, so the directory is created exactly once and every runtime
/// in this harness points at it.
struct TestEnv {
    root: PathBuf,
    /// The real agent dir captured before `PIR_AGENT_DIR` was redirected; the
    /// installed npm tree is read from here, in place and read-only.
    installed_npm_root: PathBuf,
    agent_dir: PathBuf,
    cwd: PathBuf,
}

fn test_env() -> &'static TestEnv {
    static ENV: OnceLock<TestEnv> = OnceLock::new();
    ENV.get_or_init(|| {
        let installed_agent_dir = pir::coding_agent::context::agent_dir();
        let installed_npm_root = installed_agent_dir.join("npm").join("node_modules");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "pir-ecosystem-runtime-exec-{}-{stamp}",
            std::process::id()
        ));
        let agent_dir = root.join("agent");
        let cwd = root.join("cwd");
        let config_home = root.join("config");
        std::fs::create_dir_all(&agent_dir).expect("create temp agent dir");
        std::fs::create_dir_all(&cwd).expect("create temp cwd");
        std::fs::create_dir_all(&config_home).expect("create temp XDG config home");

        // Redirect every agent-dir consumer (extension loader fallback,
        // `getAgentDir()` shim, tool-repair's config lookup) before the first
        // runtime starts. Integration tests run in their own process, so this
        // cannot leak into another test binary.
        std::env::set_var("PIR_AGENT_DIR", &agent_dir);
        // The rpiv-* packages resolve their config through `~/.config`; the
        // XDG override keeps reads off the operator's real config while
        // preserving the packages' documented lookup rule.
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        eprintln!(
            "[ecosystem_runtime_exec] temp agent dir={} cwd={} xdg_config={}",
            agent_dir.display(),
            cwd.display(),
            config_home.display()
        );
        TestEnv {
            root,
            installed_npm_root,
            agent_dir,
            cwd,
        }
    })
}

impl TestEnv {
    /// Root that holds the loaded package files, retained for diagnostics.
    #[allow(dead_code)]
    fn root(&self) -> &Path {
        &self.root
    }

    fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }
}

/// Resolve one installed package root, reporting a skip (or failing under
/// `PIR_REQUIRE_INSTALLED_EXTENSIONS=1`) when it is not installed. A missing
/// package is never a silent pass: the skip line names the package and the
/// strict-mode env var.
fn installed_package(name: &str) -> Option<PathBuf> {
    let npm_root = &test_env().installed_npm_root;
    let root = npm_root.join(name);
    if root.join("package.json").is_file() {
        return Some(root);
    }
    if std::env::var_os("PIR_REQUIRE_INSTALLED_EXTENSIONS").is_some() {
        panic!("{name} is not installed under {}", npm_root.display());
    }
    eprintln!(
        "[ecosystem_runtime_exec] SKIP package={name} reason=not installed (set \
         PIR_REQUIRE_INSTALLED_EXTENSIONS=1 to fail instead)"
    );
    None
}

fn extension_config(model: Option<&str>, session_id: &str) -> ExtensionConfig {
    let limits = ExtensionLimits {
        call_timeout: CALL_TIMEOUT,
        before_agent_start_timeout: BEFORE_AGENT_START_TIMEOUT,
        ..ExtensionLimits::default()
    };
    ExtensionConfig {
        cwd: test_env().cwd.clone(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: model.map(str::to_owned),
        session_id: Some(session_id.to_owned()),
        system_prompt: "You are a verification harness.".to_owned(),
        is_idle: true,
        limits: Some(limits),
    }
}

/// Start the real actor (bounded) and load the package root through its
/// `pi.extensions` manifest (bounded). Returns the handle, the load result and
/// the measured load time.
async fn start_and_load(
    package: &str,
    root: &Path,
    model: Option<&str>,
    session_id: &str,
) -> (ExtensionHandle, LoadResult, Duration) {
    let config = extension_config(model, session_id);
    let handle = tokio::time::timeout(
        START_BUDGET,
        tokio::task::spawn_blocking(move || ExtensionHandle::start(config)),
    )
    .await
    .unwrap_or_else(|_| panic!("{package}: ExtensionHandle::start exceeded {START_BUDGET:?}"))
    .expect("actor start task panicked")
    .unwrap_or_else(|error| panic!("{package}: actor start failed: {error}"));

    let root = root.to_path_buf();
    let load_started = Instant::now();
    let result = tokio::time::timeout(LOAD_BUDGET, handle.load(std::slice::from_ref(&root)))
        .await
        .unwrap_or_else(|_| panic!("{package}: load exceeded {LOAD_BUDGET:?}"))
        .unwrap_or_else(|error| panic!("{package}: load transport failed: {error}"));
    let load_ms = load_started.elapsed().as_millis();

    report(package, load_ms, &result, &handle);
    (handle, result, load_started.elapsed())
}

fn report(package: &str, load_ms: u128, result: &LoadResult, handle: &ExtensionHandle) {
    let tools: Vec<String> = handle
        .tool_defs()
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    let commands: Vec<String> = handle
        .command_defs()
        .iter()
        .map(|command| command.name.clone())
        .collect();
    let flags: Vec<String> = handle
        .flags()
        .iter()
        .map(|flag| flag.name.clone())
        .collect();
    let shortcuts: Vec<String> = handle
        .shortcuts()
        .iter()
        .map(|shortcut| shortcut.key.clone())
        .collect();
    let hooks: Vec<&str> = [
        "session_start",
        "session_shutdown",
        "session_compact",
        "session_tree",
        "agent_start",
        "tool_execution_end",
        "before_provider_request",
        "before_agent_start",
        "message_end",
    ]
    .into_iter()
    .filter(|event| handle.has_handlers(event))
    .collect();
    let errors: Vec<&str> = result
        .errors
        .iter()
        .map(|error| error.error.as_str())
        .collect();
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} load_ms={load_ms} loaded={} errors={errors:?} \
         tools={tools:?} commands={commands:?} flags={flags:?} shortcuts={shortcuts:?} \
         hooks={hooks:?}",
        result.loaded.len()
    );
}

fn assert_loaded_clean(package: &str, result: &LoadResult, expected_suffix: &str) {
    assert!(
        result.errors.is_empty(),
        "{package}: load errors: {:?}",
        result.errors
    );
    assert_eq!(
        result.loaded.len(),
        1,
        "{package}: expected one discovered entry, got {:?}",
        result.loaded
    );
    let path = &result.loaded[0].path;
    assert!(
        path.to_string_lossy().ends_with(expected_suffix),
        "{package}: discovered entry {} does not end with {expected_suffix}",
        path.display()
    );
}

fn assert_emit_clean(package: &str, event: &str, outcome: &pir::extensions::EmitOutcome) {
    assert!(
        outcome.errors.is_empty(),
        "{package}: `{event}` handler errors: {:?}",
        outcome.errors
    );
}

// ---------------------------------------------------------------------------
// @juicesharp/rpiv-todo — tool + command + hooks
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn rpiv_todo_executes_its_tool_command_and_hooks() {
    let package = "@juicesharp/rpiv-todo";
    let Some(root) = installed_package(package) else {
        return;
    };
    let (handle, load, _elapsed) = start_and_load(package, &root, None, "eco-rpiv-todo").await;
    assert_loaded_clean(package, &load, "index.ts");

    let tool_names: Vec<String> = handle
        .tool_defs()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        tool_names.iter().any(|name| name == "todo"),
        "{package}: `todo` tool not registered (tools={tool_names:?})"
    );
    let command_names: Vec<String> = handle
        .command_defs()
        .into_iter()
        .map(|command| command.name)
        .collect();
    assert!(
        command_names.iter().any(|name| name == "todos"),
        "{package}: `/todos` command not registered (commands={command_names:?})"
    );
    assert!(
        handle.has_handlers("session_start"),
        "{package}: session_start hook not registered"
    );
    assert!(
        handle.has_handlers("tool_execution_end"),
        "{package}: tool_execution_end hook not registered"
    );
    assert!(
        handle.has_handlers("agent_start"),
        "{package}: agent_start hook not registered"
    );

    // Representative lifecycle dispatch: the handler replays the (empty)
    // session branch and, headless, returns without touching UI.
    let mut session_start = json!({ "type": "session_start", "reason": "startup" });
    let outcome = tokio::time::timeout(
        CALL_TIMEOUT,
        handle.emit("session_start", &mut session_start),
    )
    .await
    .expect("session_start dispatch timed out")
    .expect("session_start dispatch transport");
    assert_emit_clean(package, "session_start", &outcome);

    // Tool execution through QuickJS: create a task and check the structured
    // envelope the package documents (`details.tasks`).
    let result = tokio::time::timeout(
        CALL_TIMEOUT,
        handle.call_tool(
            "todo",
            "eco-todo-call-1",
            &json!({
                "action": "create",
                "subject": "Runtime execution probe",
                "description": "Created by tests/ecosystem_runtime_exec.rs"
            }),
        ),
    )
    .await
    .expect("todo tool call timed out")
    .expect("todo tool call failed");
    assert!(
        !result.content.is_empty(),
        "{package}: todo returned no content"
    );
    assert_eq!(result.details["action"], "create", "{result:?}");
    assert_eq!(
        result.details["tasks"][0]["subject"], "Runtime execution probe",
        "{result:?}"
    );
    assert_eq!(
        result.details["tasks"][0]["status"], "pending",
        "{result:?}"
    );
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} tool=todo content={:?} details={}",
        result
            .content
            .first()
            .map(|item| item.text.as_str())
            .unwrap_or(""),
        result.details
    );

    // Slash-command routing: headless `/todos` answers through the registered
    // handler (notify "requires interactive mode") instead of "Unknown".
    let dispatched = tokio::time::timeout(CALL_TIMEOUT, handle.dispatch_command("todos", ""))
        .await
        .expect("todos command timed out")
        .expect("todos command transport");
    assert!(
        dispatched.ok,
        "{package}: /todos dispatch failed: {dispatched:?}"
    );
    let notifications = handle.drain_notifications();
    assert!(
        notifications
            .iter()
            .any(|notification| matches!(notification, Notification::Notify { .. })),
        "{package}: /todos produced no notification (notifications={notifications:?})"
    );
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} command=todos ok={} notifications={notifications:?}",
        dispatched.ok
    );
}

// ---------------------------------------------------------------------------
// pi-tool-repair — before_provider_request + message_end hooks
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn pi_tool_repair_executes_its_two_hooks() {
    let package = "pi-tool-repair";
    let Some(root) = installed_package(package) else {
        return;
    };
    // `glm-*` is in the package's default anchor-bleed model list, so the
    // hook takes its repair path instead of returning early.
    let (handle, load, _elapsed) =
        start_and_load(package, &root, Some("glm-4.6"), "eco-tool-repair").await;
    assert_loaded_clean(package, &load, "tool-repair.ts");

    assert!(
        handle.has_handlers("before_provider_request"),
        "{package}: before_provider_request hook not registered"
    );
    assert!(
        handle.has_handlers("message_end"),
        "{package}: message_end hook not registered"
    );
    assert!(
        handle.tool_defs().is_empty() && handle.command_defs().is_empty(),
        "{package}: hook-only package registered a tool/command"
    );

    // The `ctx.model` object the hook reads (`model.id`) is host-pushed state,
    // not the config string.
    handle.update_session_context(SessionContextSnapshot {
        model: json!({ "id": "glm-4.6", "provider": "zai", "name": "GLM 4.6" }),
        session_id: Some("eco-tool-repair".to_owned()),
        ..SessionContextSnapshot::default()
    });

    // before_provider_request: anchor-bleed model schemas are sanitized.
    let mut request = json!({
        "type": "before_provider_request",
        "payload": {
            "model": "glm-4.6",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "search",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "q": { "type": "string", "pattern": "^task$" }
                        }
                    }
                }
            }]
        }
    });
    let outcome = tokio::time::timeout(
        CALL_TIMEOUT,
        handle.emit("before_provider_request", &mut request),
    )
    .await
    .expect("before_provider_request dispatch timed out")
    .expect("before_provider_request dispatch transport");
    assert_emit_clean(package, "before_provider_request", &outcome);
    assert_eq!(
        outcome.result["tools"][0]["function"]["parameters"]["properties"]["q"]["pattern"], "task",
        "{package}: schema anchors were not stripped: {}",
        outcome.result
    );
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} event=before_provider_request pattern={}",
        outcome.result["tools"][0]["function"]["parameters"]["properties"]["q"]["pattern"]
    );

    // message_end: anchor bleed in tool-call arguments is repaired and the
    // replacement message is returned to the host.
    let mut message_end = json!({
        "type": "message_end",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "eco-call-1",
                "name": "bash",
                "arguments": { "command": "^echo hello$" }
            }],
            "stopReason": "toolUse"
        }
    });
    let outcome = tokio::time::timeout(CALL_TIMEOUT, handle.emit("message_end", &mut message_end))
        .await
        .expect("message_end dispatch timed out")
        .expect("message_end dispatch transport");
    assert_emit_clean(package, "message_end", &outcome);
    assert_eq!(
        outcome.result["message"]["content"][0]["arguments"]["command"], "echo hello",
        "{package}: message_end anchor repair did not run: {}",
        outcome.result
    );
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} event=message_end repaired_command={}",
        outcome.result["message"]["content"][0]["arguments"]["command"]
    );
}

// ---------------------------------------------------------------------------
// @juicesharp/rpiv-i18n — flag + command + session_start hook
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn rpiv_i18n_executes_its_flag_command_and_session_start() {
    let package = "@juicesharp/rpiv-i18n";
    let Some(root) = installed_package(package) else {
        return;
    };
    let (handle, load, _elapsed) = start_and_load(package, &root, None, "eco-rpiv-i18n").await;
    assert_loaded_clean(package, &load, "index.ts");

    let flags: Vec<(String, String)> = handle
        .flags()
        .into_iter()
        .map(|flag| (flag.name, flag.flag_type))
        .collect();
    assert!(
        flags
            .iter()
            .any(|(name, kind)| name == "locale" && kind == "string"),
        "{package}: `locale` string flag not registered (flags={flags:?})"
    );
    let commands: Vec<String> = handle
        .command_defs()
        .into_iter()
        .map(|command| command.name)
        .collect();
    assert!(
        commands.iter().any(|name| name == "languages"),
        "{package}: `/languages` command not registered (commands={commands:?})"
    );
    assert!(
        handle.has_handlers("session_start"),
        "{package}: session_start hook not registered"
    );

    // Apply the flag before the lifecycle dispatch: this drives the
    // flag -> applyLocale branch without reading the operator's locale file.
    handle
        .set_flag_value("locale", FlagValue::Str("en".to_owned()))
        .expect("locale flag accepts a string");
    let mut session_start = json!({ "type": "session_start", "reason": "startup" });
    let outcome = tokio::time::timeout(
        CALL_TIMEOUT,
        handle.emit("session_start", &mut session_start),
    )
    .await
    .expect("session_start dispatch timed out")
    .expect("session_start dispatch transport");
    assert_emit_clean(package, "session_start", &outcome);

    // The hook's effect is observable through the package's published
    // globalThis snapshot.
    let state = tokio::time::timeout(
        CALL_TIMEOUT,
        handle.eval_json("JSON.stringify(globalThis[Symbol.for('rpiv-i18n')] ?? null)"),
    )
    .await
    .expect("i18n state probe timed out")
    .expect("i18n state probe failed");
    assert_eq!(
        state["locale"], "en",
        "{package}: session_start did not apply the locale flag: {state}"
    );
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} event=session_start locale={} namespaces={}",
        state["locale"],
        state["namespaces"]
            .as_object()
            .map_or(0, serde_json::Map::len)
    );
}

// ---------------------------------------------------------------------------
// @vndv/pi-codegraph — tools + before_agent_start prompt hook
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn pi_codegraph_registers_its_tools_and_prompt_hook() {
    let package = "@vndv/pi-codegraph";
    let Some(root) = installed_package(package) else {
        return;
    };
    let (handle, load, _elapsed) = start_and_load(package, &root, None, "eco-codegraph").await;
    assert_loaded_clean(package, &load, "codegraph.ts");

    const EXPECTED: [&str; 5] = [
        "codegraph_search",
        "codegraph_callers",
        "codegraph_callees",
        "codegraph_impact",
        "codegraph_files",
    ];
    let tools: Vec<String> = handle
        .tool_defs()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    for expected in EXPECTED {
        assert!(
            tools.iter().any(|name| name == expected),
            "{package}: `{expected}` tool not registered (tools={tools:?})"
        );
    }
    assert!(
        handle.has_handlers("before_agent_start"),
        "{package}: before_agent_start hook not registered"
    );

    let mut payload = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "where is the scheduler?",
        "systemPrompt": "BASE PROMPT"
    });
    let outcome = tokio::time::timeout(
        BEFORE_AGENT_START_TIMEOUT,
        handle.emit_with_timeout(
            "before_agent_start",
            &mut payload,
            BEFORE_AGENT_START_TIMEOUT,
        ),
    )
    .await
    .expect("before_agent_start dispatch timed out")
    .expect("before_agent_start dispatch transport");
    assert_emit_clean(package, "before_agent_start", &outcome);
    let prompt = outcome.result["systemPrompt"]
        .as_str()
        .unwrap_or_else(|| panic!("{package}: no systemPrompt in {}", outcome.result));
    assert!(
        prompt.starts_with("BASE PROMPT"),
        "{package}: prompt hook dropped the host prompt: {prompt}"
    );
    assert!(
        prompt.contains("CodeGraph tools are available"),
        "{package}: prompt hook did not append its guidance: {prompt}"
    );
    eprintln!(
        "[ecosystem_runtime_exec] pkg={package} event=before_agent_start appended_chars={}",
        prompt.len() - "BASE PROMPT".len()
    );
}

// ---------------------------------------------------------------------------
// Known limitation: the excluded packages (documented, not executed)
// ---------------------------------------------------------------------------

/// The excluded packages are explicitly listed in the module docs. This test
/// exists so the exclusion is visible in `cargo test -- --list` and in the
/// run output rather than being an undocumented omission.
#[test]
#[ignore = "startup cost / network / server bind: excluded from this execution matrix; see logs/perf/ecosystem-runtime-<date>.md"]
fn excluded_packages_are_documented_not_silently_passed() {
    let env = test_env();
    assert!(
        env.agent_dir().is_dir(),
        "temp agent dir must exist for the execution matrix"
    );
}
