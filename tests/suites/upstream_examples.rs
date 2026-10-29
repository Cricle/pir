//! Load and run REAL upstream `pi` example extensions in the pir extension
//! runtime, directly from the upstream checkout, with no npm involved.
//!
//! Upstream checkout resolution: `PI_UPSTREAM_DIR`, else `<pir repo>/../pi`.
//! Examples live in `packages/coding-agent/examples/extensions/`. When the
//! checkout (or the examples directory) is absent the tests print a skip note
//! and pass, so CI without the upstream repo stays green.
//!
//! Why this proves "no npm / no node_modules":
//! - Every example below is loaded by absolute path from the checkout; the pir
//!   runtime transpiles the TS source in-process and registers it under a
//!   synthetic `pir-ext:` module name inside QuickJS. There is no node/npm
//!   process and no `node_modules` resolution anywhere in the pipeline.
//! - `assert_loaded_cleanly` asserts the loaded path lives under the checkout
//!   root and contains no `node_modules` segment.
//! - `assert_no_npm_dependencies` re-scans the example source and rejects any
//!   bare import specifier that is not one of the runtime's virtual shims and
//!   any value import that is not one of the shimmed value exports.
//!
//! Selection rule applied to the upstream examples (see the per-test comments
//! for what each one exercises). An example is eligible when its imports are
//! limited to:
//!   (a) type-only imports from `@earendil-works/*` (erased by the transpiler),
//!   (b) value imports from `typebox` / `@sinclair/typebox`, or the shimmed
//!       `@earendil-works/pi-coding-agent` value exports (`defineTool`,
//!       `isToolCallEventType`, `isToolResult`, `CONFIG_DIR_NAME`, `VERSION`,
//!       `getAgentDir`, `withFileMutationQueue`) and the `@earendil-works/pi-ai`
//!       shim (`Type`, `StringEnum`, `uuidv7` + the coding-agent re-exports),
//!   (c) relative files inside the example,
//! and it only touches implemented runtime APIs. Examples importing
//! `@earendil-works/pi-tui` (the shim throws by design), `node:*` / `fs` /
//! `path` / `child_process`, or unshimmed value exports (e.g. `BorderedLoader`,
//! `createBashTool`, `convertToLlm`) are excluded.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime, LoadResult, Notification};
use serde_json::json;

/// System prompt used by every runtime in this file. `system-prompt-header.ts`
/// reports its length, so it must stay ASCII (JS `String.length` == Rust byte
/// length only for ASCII).
const SYSTEM_PROMPT: &str = "You are a test.";

/// The upstream examples exercised by the individual tests below. Also loaded
/// together in `selected_examples_load_together_...`.
const SELECTED_EXAMPLES: &[&str] = &[
    "hello.ts",
    "commands.ts",
    "permission-gate.ts",
    "event-bus.ts",
    "inline-bash.ts",
    "dynamic-tools.ts",
    "system-prompt-header.ts",
    "shutdown-command.ts",
];

struct Upstream {
    /// Canonical root of the upstream `pi` checkout.
    root: PathBuf,
    /// `<root>/packages/coding-agent/examples/extensions`.
    examples: PathBuf,
}

/// Resolve the upstream `pi` checkout: `PI_UPSTREAM_DIR`, else `../pi`
/// relative to this crate. Returns `None` when neither location has the
/// examples directory (callers skip).
fn upstream() -> Option<Upstream> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("PI_UPSTREAM_DIR") {
        if !dir.is_empty() {
            candidates.push(PathBuf::from(dir));
        }
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("pi"),
    );

    for candidate in candidates {
        let Ok(root) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        let examples = root
            .join("packages")
            .join("coding-agent")
            .join("examples")
            .join("extensions");
        if examples.is_dir() {
            return Some(Upstream { root, examples });
        }
    }
    None
}

/// Skip the current test (pass) when the upstream checkout is unavailable —
/// unless `PIR_REQUIRE_UPSTREAM=1` is set, in which case a missing checkout
/// fails the test so CI can enforce real upstream coverage.
macro_rules! require_upstream {
    () => {
        match upstream() {
            Some(up) => up,
            None => {
                assert!(
                    std::env::var_os("PIR_REQUIRE_UPSTREAM").is_none(),
                    "PIR_REQUIRE_UPSTREAM is set but no upstream pi checkout was found; \
                     set PI_UPSTREAM_DIR or keep a `pi` checkout next to the `pir` repo"
                );
                eprintln!(
                    "SKIP: upstream pi checkout not found. Set PI_UPSTREAM_DIR or keep a \
                     `pi` checkout next to the `pir` repo (../pi). Skipping this test."
                );
                return;
            }
        }
    };
}

async fn new_runtime(up: &Upstream) -> ExtensionRuntime {
    let config = ExtensionConfig {
        cwd: up.examples.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("upstream-examples".to_string()),
        session_id: Some("upstream-examples-session".to_string()),
        system_prompt: SYSTEM_PROMPT.to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    };
    ExtensionRuntime::new(config)
        .await
        .expect("extension runtime construction")
}

/// Common load assertions: zero errors, exactly `expected` extensions, the
/// requested file loaded directly from the checkout (no `node_modules`), and
/// no unsupported imports in its source.
fn assert_loaded_cleanly(up: &Upstream, result: &LoadResult, path: &Path, expected: usize) {
    assert!(
        result.errors.is_empty(),
        "load errors for {}: {:?}",
        path.display(),
        result.errors
    );
    assert_eq!(
        result.loaded.len(),
        expected,
        "unexpected loaded set for {}: {:?}",
        path.display(),
        result.loaded
    );
    let ext = result
        .loaded
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{} not among loaded extensions", path.display()));
    assert!(
        ext.path.starts_with(&up.root),
        "{} was not loaded directly from the pi checkout {}",
        ext.path.display(),
        up.root.display()
    );
    assert!(
        !ext.path.to_string_lossy().contains("node_modules"),
        "{} unexpectedly references node_modules",
        ext.path.display()
    );
    assert!(
        ext.module_name.starts_with("pir-ext:"),
        "expected a synthetic pir-ext module name, got {}",
        ext.module_name
    );
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    assert!(
        ext.module_name.ends_with(file_name),
        "module name {} does not correspond to {}",
        ext.module_name,
        path.display()
    );

    let source = std::fs::read_to_string(path).expect("read example source");
    assert_no_npm_dependencies(path, &source);
}

/// Value exports provided by the runtime's virtual shim for a bare specifier.
/// `None` means the specifier is not importable in the runtime at all.
fn shimmed_value_exports(specifier: &str) -> Option<&'static [&'static str]> {
    const CODING_AGENT: &[&str] = &[
        "defineTool",
        "isToolCallEventType",
        "isToolResult",
        "CONFIG_DIR_NAME",
        "VERSION",
        "getAgentDir",
        "withFileMutationQueue",
    ];
    const PI_AI: &[&str] = &[
        "defineTool",
        "isToolCallEventType",
        "isToolResult",
        "CONFIG_DIR_NAME",
        "VERSION",
        "getAgentDir",
        "withFileMutationQueue",
        "Type",
        "StringEnum",
        "uuidv7",
    ];
    const TYPEBOX: &[&str] = &["Type", "Compile", "Value"];
    match specifier {
        "@earendil-works/pi-coding-agent" | "@mariozechner/pi-coding-agent" => Some(CODING_AGENT),
        "@earendil-works/pi-ai" | "@earendil-works/pi-ai/compat" | "@mariozechner/pi-ai" => {
            Some(PI_AI)
        }
        "@earendil-works/pi-agent-core" | "@mariozechner/pi-agent-core" => Some(CODING_AGENT),
        "typebox"
        | "typebox/compile"
        | "typebox/value"
        | "@sinclair/typebox"
        | "@sinclair/typebox/compile"
        | "@sinclair/typebox/value" => Some(TYPEBOX),
        _ => None,
    }
}

/// Mechanically enforces the selection rule: only relative imports and runtime
/// virtual-module specifiers are permitted, and every value import must be one
/// of the shimmed value exports. Type-only imports are erased by the
/// transpiler and therefore skipped.
fn assert_no_npm_dependencies(path: &Path, source: &str) {
    for line in source.lines() {
        let trimmed = line.trim_start();
        let is_import = trimmed.starts_with("import ");
        let is_export_from = trimmed.starts_with("export ") && trimmed.contains(" from ");
        if !is_import && !is_export_from {
            continue;
        }
        let Some(from_idx) = trimmed.find(" from \"") else {
            continue;
        };
        let rest = &trimmed[from_idx + " from \"".len()..];
        let Some(end_idx) = rest.find('"') else {
            continue;
        };
        let specifier = &rest[..end_idx];
        if specifier.starts_with('.') {
            continue; // relative file import
        }
        let Some(allowed) = shimmed_value_exports(specifier) else {
            panic!(
                "{}: unsupported bare import {specifier:?}; the pir extension runtime has no npm",
                path.display()
            );
        };
        if trimmed.starts_with("import type ") {
            continue; // entire import is erased at transpile time
        }
        if let Some(open) = trimmed.find('{') {
            if let Some(close) = trimmed[open + 1..].find('}') {
                let names = &trimmed[open + 1..open + 1 + close];
                for item in names.split(',') {
                    let item = item.trim();
                    if item.is_empty() || item.starts_with("type ") {
                        continue; // inline `type X` specifier is erased
                    }
                    let name = item.split_whitespace().next().unwrap_or(item);
                    assert!(
                        allowed.contains(&name),
                        "{}: value import {name:?} from {specifier:?} is not provided by the runtime shim",
                        path.display()
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hello.ts
// Exercises: `defineTool` from the coding-agent shim, `Type` re-exported by
// the pi-ai shim (typebox under the hood), tool registration at factory time,
// the TypeBox schema reaching `ToolDef.parameters`, and `call_tool` executing
// the upstream TS `execute` with content/details crossing back to the host.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_hello_tool_registers_and_executes() {
    let up = require_upstream!();
    let path = up.examples.join("hello.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load hello.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    let tools = rt.tool_defs();
    let tool = tools
        .iter()
        .find(|t| t.name == "hello")
        .expect("hello tool registered at load time");
    assert_eq!(tool.label.as_deref(), Some("Hello"));
    assert_eq!(tool.description, "A simple greeting tool");
    assert_eq!(tool.parameters["type"], "object");
    assert_eq!(tool.parameters["properties"]["name"]["type"], "string");
    assert_eq!(
        tool.parameters["properties"]["name"]["description"],
        "Name to greet"
    );
    assert_eq!(tool.parameters["required"], json!(["name"]));

    let out = rt
        .call_tool("hello", "call-hello", &json!({ "name": "World" }))
        .await
        .expect("hello tool executes");
    assert_eq!(out.content.len(), 1);
    assert_eq!(out.content[0].kind, "text");
    assert_eq!(out.content[0].text, "Hello, World!");
    assert_eq!(out.details["greeted"], "World");
}

// ---------------------------------------------------------------------------
// permission-gate.ts
// Exercises: `tool_call` hooks blocking dangerous bash commands in headless
// mode (no UI), reading `event.toolName` / `event.input.command`, regex state
// across handler invocations, and the merged `{ block, reason }` outcome.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_permission_gate_blocks_dangerous_bash() {
    let up = require_upstream!();
    let path = up.examples.join("permission-gate.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load permission-gate.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    for (id, command) in [
        ("t1", "rm -rf ./build"),
        ("t2", "sudo apt-get install foo"),
        ("t3", "chmod 777 ./script.sh"),
    ] {
        let mut payload = json!({
            "type": "tool_call",
            "toolCallId": id,
            "toolName": "bash",
            "input": { "command": command },
        });
        let outcome = rt.emit("tool_call", &mut payload).await.expect("emit");
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.result["block"], true, "command: {command}");
        assert_eq!(
            outcome.result["reason"], "Dangerous command blocked (no UI for confirmation)",
            "command: {command}"
        );
    }

    // Safe command: no block.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "t4",
        "toolName": "bash",
        "input": { "command": "ls -la" },
    });
    let outcome = rt.emit("tool_call", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result, serde_json::Value::Null);

    // Non-bash tools are ignored entirely.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "t5",
        "toolName": "read",
        "input": { "path": "./x" },
    });
    let outcome = rt.emit("tool_call", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result, serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// dynamic-tools.ts
// Exercises: a value import from bare `typebox`, tool registration inside a
// `session_start` handler (registry refresh after dispatch), `ctx.ui.notify`
// capture, command dispatch running an upstream `handler`, and a command that
// registers a NEW tool at runtime which is then callable through the host.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_dynamic_tools_register_on_session_start_and_via_command() {
    let up = require_upstream!();
    let path = up.examples.join("dynamic-tools.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load dynamic-tools.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    // The session tool does not exist before session_start.
    assert!(rt.tool_defs().iter().all(|t| t.name != "echo_session"));
    assert!(rt.command_defs().iter().any(|c| c.name == "add-echo-tool"));

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let tools = rt.tool_defs();
    let echo = tools
        .iter()
        .find(|t| t.name == "echo_session")
        .expect("session_start registered echo_session");
    assert_eq!(echo.description, "Echo a message with prefix: [session] ");
    assert_eq!(echo.parameters["type"], "object");
    assert_eq!(echo.parameters["properties"]["message"]["type"], "string");
    assert_eq!(
        echo.parameters["properties"]["message"]["description"],
        "Message to echo"
    );
    assert_eq!(echo.parameters["required"], json!(["message"]));

    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, level }
                if message == "Registered dynamic tool: echo_session" && level == "info"
        )),
        "{notifications:?}"
    );

    let out = rt
        .call_tool("echo_session", "call-dyn-1", &json!({ "message": "hi" }))
        .await
        .expect("echo_session executes");
    assert_eq!(out.content[0].text, "[session] hi");
    assert_eq!(out.details["tool"], "echo_session");
    assert_eq!(out.details["prefix"], "[session] ");

    // /add-echo-tool registers a new tool at runtime.
    let outcome = rt
        .dispatch_command("add-echo-tool", "my_tool")
        .await
        .expect("dispatch add-echo-tool");
    assert!(outcome.ok, "{:?}", outcome.error);
    assert!(rt.tool_defs().iter().any(|t| t.name == "my_tool"));
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, level }
                if message == "Registered dynamic tool: my_tool" && level == "info"
        )),
        "{notifications:?}"
    );

    let out = rt
        .call_tool("my_tool", "call-dyn-2", &json!({ "message": "dyn" }))
        .await
        .expect("my_tool executes");
    assert_eq!(out.content[0].text, "[my_tool] dyn");

    // Invalid name: usage warning, no tool registered.
    let outcome = rt
        .dispatch_command("add-echo-tool", "Not A Tool!")
        .await
        .expect("dispatch invalid name");
    assert!(outcome.ok, "{:?}", outcome.error);
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, level }
                if message == "Usage: /add-echo-tool <tool_name> (lowercase, numbers, underscores)"
                    && level == "warning"
        )),
        "{notifications:?}"
    );

    // Duplicate name: warning, still only one registration.
    let outcome = rt
        .dispatch_command("add-echo-tool", "my_tool")
        .await
        .expect("dispatch duplicate");
    assert!(outcome.ok, "{:?}", outcome.error);
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, level }
                if message == "Tool already registered: my_tool" && level == "warning"
        )),
        "{notifications:?}"
    );
}

// ---------------------------------------------------------------------------
// event-bus.ts
// Exercises: `pi.events.on` bus subscription at factory time, a session_start
// handler that stores the ctx and emits on the bus, a second handler emitting
// during the SAME dispatch (handlers run in registration order), and a slash
// command whose handler emits on the bus; the listener's `ctx.ui.notify` calls
// are observed as host notifications.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_event_bus_notifies_via_pi_events() {
    let up = require_upstream!();
    let path = up.examples.join("event-bus.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load event-bus.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);
    assert!(rt.command_defs().iter().any(|c| c.name == "emit"));

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, level }
                if message == "Event from event-bus-example: Session started" && level == "info"
        )),
        "{notifications:?}"
    );

    let outcome = rt
        .dispatch_command("emit", "custom message")
        .await
        .expect("dispatch /emit");
    assert!(outcome.ok, "{:?}", outcome.error);
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, level }
                if message == "Event from /emit command: custom message" && level == "info"
        )),
        "{notifications:?}"
    );
}

// ---------------------------------------------------------------------------
// commands.ts
// Exercises: a command registered with only a `handler` (plus an unused
// `getArgumentCompletions`), `pi.getCommands()` served from the host command
// registry across the JS boundary, and the no-op `ctx.ui.select` fallback
// driving the handler onto its notify path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_commands_extension_lists_via_get_commands() {
    let up = require_upstream!();
    let path = up.examples.join("commands.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load commands.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    let cmd = rt
        .command_defs()
        .into_iter()
        .find(|c| c.name == "commands")
        .expect("commands slash command registered");
    assert_eq!(
        cmd.description.as_deref(),
        Some("List available slash commands")
    );

    // Host commands carry `source: "extension"`, so the source filter finds
    // commands and the flow continues to the selector (headless no-op)
    // instead of reporting "No extension commands found".
    let outcome = rt
        .dispatch_command("commands", "extension")
        .await
        .expect("dispatch /commands extension");
    assert!(outcome.ok, "{:?}", outcome.error);
    let notifications = rt.drain_notifications();
    assert!(
        !notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, .. }
                if message.contains("No extension commands found")
        )),
        "`source` must be present so the filter matches: {notifications:?}"
    );

    // With no filter the selector (no-op, returns undefined) ends the handler
    // silently after listing.
    let outcome = rt
        .dispatch_command("commands", "")
        .await
        .expect("dispatch /commands");
    assert!(outcome.ok, "{:?}", outcome.error);
    assert!(
        rt.drain_notifications().is_empty(),
        "no notifications expected on the no-filter path"
    );
}

// ---------------------------------------------------------------------------
// system-prompt-header.ts
// Exercises: `ctx.getSystemPrompt()` returning the host-configured prompt,
// `ctx.ui.setStatus` recorded as real Status notifications (text set on
// agent_start, cleared with `undefined` on session_shutdown).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_system_prompt_header_reports_via_set_status() {
    let up = require_upstream!();
    let path = up.examples.join("system-prompt-header.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load system-prompt-header.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    let mut payload = json!({});
    let outcome = rt.emit("agent_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let expected_status = format!("System: {} chars", SYSTEM_PROMPT.len());
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Status { key, text }
                if key == "system-prompt" && text.as_deref() == Some(expected_status.as_str())
        )),
        "{notifications:?}"
    );

    let mut payload = json!({});
    let outcome = rt
        .emit("session_shutdown", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Status { key, text }
                if key == "system-prompt" && text.is_none()
        )),
        "{notifications:?}"
    );
}

// ---------------------------------------------------------------------------
// inline-bash.ts
// Exercises: the host-backed `pi.exec` real subprocess API (spawn, args,
// timeout option) driven from an `input` handler, plus the `input` event
// `{ action: "transform" }` outcome merging the rewritten text back to the
// host. The assertion is robust to whether a `bash` executable exists: the
// `!{...}` pattern must be replaced by either command output or the spawn
// error message.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_inline_bash_expands_via_pi_exec() {
    let up = require_upstream!();
    let path = up.examples.join("inline-bash.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load inline-bash.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    let mut payload = json!({
        "type": "input",
        "text": "The answer is !{echo hi}",
        "source": "interactive",
    });
    let outcome = rt.emit("input", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result["action"], "transform");

    let text = outcome.result["text"]
        .as_str()
        .expect("transform carries text");
    assert!(
        !text.contains("!{echo hi}"),
        "inline bash pattern must be replaced by exec output or its error, got {text:?}"
    );
    assert!(
        text.starts_with("The answer is "),
        "only the pattern should be replaced, got {text:?}"
    );

    // A whole-line bash prompt (`!cmd`) is passed through untouched.
    let mut payload = json!({
        "type": "input",
        "text": "!git status",
        "source": "interactive",
    });
    let outcome = rt.emit("input", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    // The handler returns `{ action: "continue" }`, which the runtime does not
    // surface as a transform outcome.
    assert_eq!(outcome.result, serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// shutdown-command.ts
// Exercises: bare `typebox` value import alongside a type-only import, two
// `pi.registerTool` calls with TypeBox schemas (`Type.Object({})` with no
// properties and one with a required parameter), `ctx.shutdown()` surfacing as
// a host-level `ShutdownRequest` notification from both a tool and a `/quit`
// command (upstream `shutdownRequested`; it must not abort the run), and
// `onUpdate(...)` progress callbacks surfacing as ToolUpdate notifications
// with partial JSON payloads.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upstream_shutdown_command_executes_tools_and_aborts() {
    let up = require_upstream!();
    let path = up.examples.join("shutdown-command.ts");

    let rt = new_runtime(&up).await;
    let result = rt
        .load(std::slice::from_ref(&path))
        .await
        .expect("load shutdown-command.ts");
    assert_loaded_cleanly(&up, &result, &path, 1);

    let tools = rt.tool_defs();
    let finish = tools
        .iter()
        .find(|t| t.name == "finish_and_exit")
        .expect("finish_and_exit registered");
    assert_eq!(finish.parameters["type"], "object");
    // `Type.Object({})` has no populated properties, so no `required` array.
    assert!(finish.parameters.get("required").is_none());
    assert!(tools.iter().any(|t| t.name == "deploy_and_exit"));
    assert!(rt.command_defs().iter().any(|c| c.name == "quit"));

    // A no-parameter tool still executes end to end and requests shutdown.
    let out = rt
        .call_tool("finish_and_exit", "call-finish", &json!({}))
        .await
        .expect("finish_and_exit executes");
    assert_eq!(
        out.content[0].text,
        "Shutdown requested. Exiting after this response."
    );
    let notifications = rt.drain_notifications();
    assert!(
        notifications
            .iter()
            .any(|n| matches!(n, Notification::ShutdownRequest)),
        "{notifications:?}"
    );

    let out = rt
        .call_tool(
            "deploy_and_exit",
            "call-deploy",
            &json!({ "environment": "staging" }),
        )
        .await
        .expect("deploy_and_exit executes");
    assert_eq!(out.content[0].text, "Done! Shutdown requested.");
    assert_eq!(out.details["environment"], "staging");

    let notifications = rt.drain_notifications();
    let partial_updates: Vec<serde_json::Value> = notifications
        .iter()
        .filter_map(|n| match n {
            Notification::ToolUpdate {
                tool_call_id,
                partial,
                ..
            } if tool_call_id == "call-deploy" => partial.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(partial_updates.len(), 2, "{notifications:?}");
    assert_eq!(
        partial_updates[0]["content"][0]["text"], "Deploying to staging...",
        "{partial_updates:?}"
    );
    assert!(
        notifications
            .iter()
            .any(|n| matches!(n, Notification::ShutdownRequest)),
        "{notifications:?}"
    );

    let outcome = rt
        .dispatch_command("quit", "")
        .await
        .expect("dispatch /quit");
    assert!(outcome.ok, "{:?}", outcome.error);
    let notifications = rt.drain_notifications();
    assert!(
        notifications
            .iter()
            .any(|n| matches!(n, Notification::ShutdownRequest)),
        "{notifications:?}"
    );
}

// ---------------------------------------------------------------------------
// Aggregate: all selected upstream examples load in ONE runtime from the
// checkout (no npm), coexist without registration conflicts, and react to a
// single session_start dispatch driven from two different extensions.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn selected_examples_load_together_from_checkout_without_npm() {
    let up = require_upstream!();

    let paths: Vec<PathBuf> = SELECTED_EXAMPLES
        .iter()
        .map(|n| up.examples.join(n))
        .collect();
    for p in &paths {
        assert!(p.is_file(), "expected upstream example at {}", p.display());
    }

    let rt = new_runtime(&up).await;
    let result = rt.load(&paths).await.expect("load all selected examples");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        result.loaded.len(),
        SELECTED_EXAMPLES.len(),
        "{:?}",
        result.errors
    );
    assert_eq!(rt.loaded().len(), SELECTED_EXAMPLES.len());

    for ext in &result.loaded {
        assert!(
            ext.path.starts_with(&up.root),
            "{} was not loaded from the pi checkout {}",
            ext.path.display(),
            up.root.display()
        );
        assert!(!ext.path.to_string_lossy().contains("node_modules"));
        assert!(ext.module_name.starts_with("pir-ext:"));
        assert!(ext.path.to_string_lossy().ends_with(".ts"));
    }

    // All four slash commands coexist.
    let commands = rt.command_defs();
    let command_names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    for expected in ["commands", "emit", "add-echo-tool", "quit"] {
        assert!(
            command_names.contains(&expected),
            "missing command {expected:?} in {command_names:?}"
        );
    }

    // One session_start dispatch drives both dynamic-tools.ts (tool
    // registration) and event-bus.ts (bus emit -> notify).
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(rt.tool_defs().iter().any(|t| t.name == "echo_session"));
    assert!(rt.tool_defs().iter().any(|t| t.name == "hello"));
    let notifications = rt.drain_notifications();
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, .. }
                if message == "Registered dynamic tool: echo_session"
        )),
        "{notifications:?}"
    );
    assert!(
        notifications.iter().any(|n| matches!(
            n,
            Notification::Notify { message, .. }
                if message == "Event from event-bus-example: Session started"
        )),
        "{notifications:?}"
    );
}
