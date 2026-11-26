//! Integration tests for the pi-compatible JS extension runtime.
//!
//! Covers: TS transpile + load in QuickJS, tool registration + execution,
//! `tool_call` blocking, command registration, flag registration, `exec`,
//! error isolation, invalid sources and the TypeBox shim's JSON schema.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime, FlagValue, Notification};
use serde_json::json;

/// Fixture extensions are generated at runtime into a per-test temp directory:
/// the repository hygiene gate forbids tracked `.ts` files, so on-disk
/// fixtures cannot be committed and a fresh clone would otherwise fail.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-ext-fixtures-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

const THROWING_TS: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("session_start", () => {
		// never reached
	});
	throw new Error("intentional factory failure");
}
"#;

const BROKEN_TS: &str = "// Invalid TypeScript: parse must fail and surface as a load error entry.\nconst x: : = ] not typescript {";

const PERMISSION_GATE_TS: &str = r#"
/**
 * Permission Gate Extension (ported from upstream pi examples).
 *
 * Prompts for confirmation before running potentially dangerous bash
 * commands; blocks by default in headless mode.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	const dangerousPatterns = [/\brm\s+(-rf?|--recursive)/i, /\bsudo\b/i, /\b(chmod|chown)\b.*777/i];

	pi.on("tool_call", async (event, ctx) => {
		if (event.toolName !== "bash") return undefined;

		const command = event.input.command as string;
		const isDangerous = dangerousPatterns.some((p) => p.test(command));

		if (isDangerous) {
			if (!ctx.hasUI) {
				return { block: true, reason: "Dangerous command blocked (no UI for confirmation)" };
			}

			const choice = await ctx.ui.select(`Allow? ${command}`, ["Yes", "No"]);

			if (choice !== "Yes") {
				return { block: true, reason: "Blocked by user" };
			}
		}

		return undefined;
	});
}
"#;

const DYNAMIC_TOOLS_TS: &str = r#"
/**
 * Dynamic Tools Extension (ported from upstream pi examples).
 *
 * Registers an echo tool during session_start and a /add-echo-tool command
 * for registering more tools at runtime.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const ECHO_PARAMS = Type.Object({
	message: Type.String({ description: "Message to echo" }),
});

function normalizeToolName(input: string): string | undefined {
	const trimmed = input.trim().toLowerCase();
	if (!trimmed) return undefined;
	if (!/^[a-z0-9_]+$/.test(trimmed)) return undefined;
	return trimmed;
}

export default function dynamicToolsExtension(pi: ExtensionAPI) {
	const registeredToolNames = new Set<string>();

	const registerEchoTool = (name: string, label: string, prefix: string): boolean => {
		if (registeredToolNames.has(name)) {
			return false;
		}

		registeredToolNames.add(name);
		pi.registerTool({
			name,
			label,
			description: `Echo a message with prefix: ${prefix}`,
			promptSnippet: `Echo back user-provided text with ${prefix.trim()} prefix`,
			promptGuidelines: ["Use echo_session when the user asks for exact echo output."],
			parameters: ECHO_PARAMS,
			async execute(_toolCallId, params) {
				return {
					content: [{ type: "text", text: `${prefix}${params.message}` }],
					details: { tool: name, prefix },
				};
			},
		});

		return true;
	};

	pi.on("session_start", (_event, ctx) => {
		registerEchoTool("echo_session", "Echo Session", "[session] ");
		ctx.ui.notify("Registered dynamic tool: echo_session", "info");
	});

	pi.registerCommand("add-echo-tool", {
		description: "Register a new echo tool dynamically: /add-echo-tool <tool_name>",
		handler: async (args, ctx) => {
			const toolName = normalizeToolName(args);
			if (!toolName) {
				ctx.ui.notify("Usage: /add-echo-tool <tool_name>", "warning");
				return;
			}

			const created = registerEchoTool(toolName, `Echo ${toolName}`, `[${toolName}] `);
			if (!created) {
				ctx.ui.notify(`Tool already registered: ${toolName}`, "warning");
				return;
			}

			ctx.ui.notify(`Registered dynamic tool: ${toolName}`, "info");
		},
	});

	pi.registerFlag("echo-uppercase", {
		description: "Uppercase echoed messages",
		type: "boolean",
		default: false,
	});
}
"#;

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
        allow_process_exit: false,
        agent_dir: None,
    }
}

fn test_config() -> ExtensionConfig {
    test_config_at(&std::env::temp_dir())
}

async fn new_runtime() -> ExtensionRuntime {
    ExtensionRuntime::new(test_config())
        .await
        .expect("runtime construction")
}

#[tokio::test(flavor = "current_thread")]
async fn extension_runtime_cannot_reach_hard_process_exit_bridge() {
    let rt = new_runtime().await;
    // `eval_json` returns the parsed JSON value, so a `JSON.stringify(..)`
    // result is a JSON string (not a Rust `String`).
    let exit_bridge_type = rt
        .eval_json("JSON.stringify(typeof globalThis.__pir_host.exitProcess)")
        .await
        .expect("inspect host surface");
    assert_eq!(exit_bridge_type, serde_json::json!("undefined"));

    let process_exit = rt
        .eval_json(
            "JSON.stringify((() => { try { process.exit(0); } catch (_) { return 'throws'; } })())",
        )
        .await
        .expect("extension process.exit remains catchable");
    assert_eq!(process_exit, serde_json::json!("throws"));
}

#[tokio::test(flavor = "current_thread")]
async fn spike_ts_extension_registers_session_start_hook() {
    let cwd = write_temp_fixture("spike", &[]);
    let rt = ExtensionRuntime::new(test_config_at(&cwd))
        .await
        .expect("runtime construction");
    rt.seed_module(
        "pir-ext:spike.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (event, ctx) => {
                globalThis.__spike_seen = { reason: event.reason, cwd: ctx.cwd };
            });
        };
        "#,
    )
    .expect("seed");

    // Load via a seeded module name instead of a file path.
    let loaded = rt
        .seeded_load("pir-ext:spike.ts")
        .await
        .expect("load seeded extension");
    assert_eq!(loaded, 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let seen: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__spike_seen)")
        .await
        .expect("eval");
    assert_eq!(seen["reason"], "startup");
    assert_eq!(seen["cwd"], cwd.display().to_string());
}

// ---------------------------------------------------------------------------
// `pi.on` fail-closed event-name validation
// ---------------------------------------------------------------------------

/// Unknown/newer SDK events are accepted at load and warned once; the host
/// simply has no dispatch route for an event it does not know yet.
#[tokio::test(flavor = "current_thread")]
async fn pi_on_accepts_unknown_event_names_at_load() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:unknown-event.ts",
        r#"
        export default (pi) => {
            pi.on("compaction_ends", () => { globalThis.__never = true; });
        };
        "#,
    )
    .expect("seed");
    let loaded = rt
        .seeded_load("pir-ext:unknown-event.ts")
        .await
        .expect("unknown future event must not block extension loading");
    assert_eq!(loaded, 1);
}

/// Every documented family stays accepted: upstream names, the session-event
/// bridge names, pir synthics, native-only presentation names and the
/// in-tree test synthetics. `pi.events.on` (the custom in-process bus) is not
/// affected.
#[tokio::test(flavor = "current_thread")]
async fn pi_on_accepts_documented_and_synthetic_event_names() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:known-events.ts",
        r#"
        export default (pi) => {
            for (const name of [
                "session_start",
                "message_update",
                "tool_result",
                "entry_appended",
                "thinking_level_changed",
                "summarization_retry_scheduled",
                "summarization_retry_attempt_start",
                "summarization_retry_finished",
                "run_failed",
                "provider:refreshModels",
                "status",
                "extension_ui_request",
                "gate_probe",
                "probe_event",
                "q3_large_payload",
                "late_event",
            ]) {
                pi.on(name, () => {});
            }
            pi.events.on("extension-private-event", () => {});
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:known-events.ts")
        .await
        .expect("all documented names must remain registrable");
    let counts: serde_json::Value = rt
        .eval_json("__pir_handlerCountsJson()")
        .await
        .expect("handler counts");
    assert_eq!(
        counts["late_event"],
        json!(1),
        "the synthetic test-only name must reach the handler table: {counts}"
    );
}

// ---------------------------------------------------------------------------
// `pi.on("terminal_input")` — the dedicated raw-input dispatch list
// ---------------------------------------------------------------------------

/// The terminal-input bridge reads `state.terminalInputHandlers`, a list shared
/// with `ctx.ui.onTerminalInput`; generic event handlers live per extension in
/// `ext.handlers`. Before this regression `pi.on("terminal_input", ...)` only
/// filled the latter, so the handler was accepted (the name is in the central
/// registry) but never called. Registration, dispatch, consume and unsubscribe
/// must all work through the shared list.
#[tokio::test(flavor = "current_thread")]
async fn pi_on_terminal_input_registers_on_the_terminal_input_dispatch_list() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:terminal-input.ts",
        r#"
        export default (pi) => {
            globalThis.__terminal_seen = [];
            const unsubscribe = pi.on("terminal_input", (data) => {
                globalThis.__terminal_seen.push(data);
                if (data === "!") return { consume: true };
                return undefined;
            });
            globalThis.__terminal_unsubscribe = unsubscribe;
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:terminal-input.ts")
        .await
        .expect("pi.on(\"terminal_input\") must load");

    // The fast-path count and the dispatch list move together: a session with
    // no JS terminal handler must not pay the mailbox round-trip.
    let counts: serde_json::Value = rt
        .eval_json("__pir_handlerCountsJson()")
        .await
        .expect("handler counts");
    assert_eq!(
        counts["terminal_input"],
        json!(1),
        "pi.on must publish the terminal-input handler count: {counts}"
    );

    let mut payload = json!({ "data": "!" });
    let outcome = rt
        .emit("terminal_input", &mut payload)
        .await
        .expect("terminal_input dispatch");
    assert!(
        outcome.errors.is_empty(),
        "the handler ran without errors: {:?}",
        outcome.errors
    );
    assert_eq!(
        outcome.result,
        json!({ "consume": true, "data": "!" }),
        "the handler's {{consume}} decision must reach the host"
    );
    let seen: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__terminal_seen)")
        .await
        .expect("eval terminal seen");
    assert_eq!(
        seen,
        json!(["!"]),
        "the pi.on handler must receive the raw chunk"
    );

    // Same unsubscribe semantics as every other pi.on handler: idempotent,
    // removes the handler, and the next dispatch sees nothing.
    rt.eval_json("__terminal_unsubscribe(); __terminal_unsubscribe(); JSON.stringify(globalThis.__terminal_seen)")
        .await
        .expect("unsubscribe");
    let counts: serde_json::Value = rt
        .eval_json("__pir_handlerCountsJson()")
        .await
        .expect("handler counts after unsubscribe");
    assert!(
        counts.get("terminal_input").is_none(),
        "unsubscribe must clear the published count: {counts}"
    );
    let mut payload = json!({ "data": "?" });
    let outcome = rt
        .emit("terminal_input", &mut payload)
        .await
        .expect("terminal_input dispatch after unsubscribe");
    assert_eq!(
        outcome.result,
        json!(null),
        "an unsubscribed handler must not decide the input"
    );
    let seen: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__terminal_seen)")
        .await
        .expect("eval terminal seen after unsubscribe");
    assert_eq!(
        seen,
        json!(["!"]),
        "an unsubscribed handler must not be called again"
    );
}

// ---------------------------------------------------------------------------
// Discovery + load from fixtures
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn load_fixtures_with_error_isolation() {
    let rt = new_runtime().await;

    let dir = write_temp_fixture(
        "error-isolation",
        &[
            ("throwing.ts", THROWING_TS),
            ("dynamic-tools.ts", DYNAMIC_TOOLS_TS),
        ],
    );
    let result = rt
        .load(&[dir.join("throwing.ts"), dir.join("dynamic-tools.ts")])
        .await
        .expect("load returns");

    // The throwing extension produced an error entry; the other still loaded.
    assert_eq!(result.loaded.len(), 1, "{:?}", result.errors);
    assert!(result.loaded[0].path.ends_with("dynamic-tools.ts"));
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].path.ends_with("throwing.ts"));
    assert!(result.errors[0]
        .error
        .contains("intentional factory failure"));
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_ts_produces_error_entry_not_panic() {
    let rt = new_runtime().await;
    let dir = write_temp_fixture("invalid-ts", &[("broken.ts", BROKEN_TS)]);
    let result = rt.load(&[dir.join("broken.ts")]).await.expect("load");
    assert!(result.loaded.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0].error.contains("parse error"),
        "{:?}",
        result.errors
    );
}

// ---------------------------------------------------------------------------
// Tool registration + execution
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn session_start_registers_tool_and_it_executes() {
    let rt = new_runtime().await;
    let dir = write_temp_fixture("dynamic-tools", &[("dynamic-tools.ts", DYNAMIC_TOOLS_TS)]);
    let result = rt
        .load(&[dir.join("dynamic-tools.ts")])
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.loaded.len(), 1);

    // Before session_start the tool is not registered yet (registered by the
    // session_start hook).
    assert!(rt.tool_defs().iter().all(|t| t.name != "echo_session"));

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let tools = rt.tool_defs();
    let tool = tools
        .iter()
        .find(|t| t.name == "echo_session")
        .expect("tool registered");
    assert_eq!(tool.description, "Echo a message with prefix: [session] ");

    // TypeBox shim produced a valid JSON schema.
    assert_eq!(tool.parameters["type"], "object");
    assert_eq!(tool.parameters["properties"]["message"]["type"], "string");
    assert_eq!(
        tool.parameters["properties"]["message"]["description"],
        "Message to echo"
    );
    assert_eq!(tool.parameters["required"], json!(["message"]));

    // Execute from the registry.
    let tool_result = rt
        .call_tool(
            "echo_session",
            "call-1",
            &json!({ "message": "hello world" }),
        )
        .await
        .expect("tool executes");
    assert_eq!(tool_result.content.len(), 1);
    assert_eq!(tool_result.content[0].kind, "text");
    assert_eq!(tool_result.content[0].text, "[session] hello world");
    assert_eq!(tool_result.details["tool"], "echo_session");

    // The session_start hook's ctx.ui.notify was captured.
    let notifications = rt.drain_notifications();
    assert!(notifications.iter().any(|n| matches!(
        n,
        Notification::Notify { message, level }
            if message.contains("Registered dynamic tool: echo_session") && level == "info"
    )));
}

// ---------------------------------------------------------------------------
// tool_call blocking (permission-gate)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn tool_call_blocking() {
    let rt = new_runtime().await;
    let dir = write_temp_fixture(
        "permission-gate",
        &[("permission-gate.ts", PERMISSION_GATE_TS)],
    );
    let result = rt
        .load(&[dir.join("permission-gate.ts")])
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    // Dangerous command in headless mode → blocked.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "t1",
        "toolName": "bash",
        "input": { "command": "rm -rf ./build" },
    });
    let outcome = rt.emit("tool_call", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result["block"], true);
    assert_eq!(
        outcome.result["reason"],
        "Dangerous command blocked (no UI for confirmation)"
    );

    // Safe command → no block.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "t2",
        "toolName": "bash",
        "input": { "command": "ls -la" },
    });
    let outcome = rt.emit("tool_call", &mut payload).await.expect("emit");
    assert_eq!(outcome.result, serde_json::Value::Null);

    // In-place args mutation: handler rewrites event.input.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "t3",
        "toolName": "bash",
        "input": { "command": "sudo rm -rf /" },
    });
    let _ = rt.emit("tool_call", &mut payload).await.expect("emit");
    assert_eq!(payload["input"]["command"], "sudo rm -rf /");
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn command_registration_and_dispatch() {
    let rt = new_runtime().await;
    let dir = write_temp_fixture(
        "command-registration",
        &[("dynamic-tools.ts", DYNAMIC_TOOLS_TS)],
    );
    let result = rt
        .load(&[dir.join("dynamic-tools.ts")])
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let commands = rt.command_defs();
    let cmd = commands
        .iter()
        .find(|c| c.name == "add-echo-tool")
        .expect("command registered");
    assert_eq!(
        cmd.source_info.path,
        dir.join("dynamic-tools.ts").display().to_string()
    );

    let outcome = rt
        .dispatch_command("add-echo-tool", "my_tool")
        .await
        .expect("dispatch");
    assert!(outcome.ok, "{:?}", outcome.error);

    // The command registered a new tool at runtime.
    let tools = rt.tool_defs();
    assert!(tools.iter().any(|t| t.name == "my_tool"));

    let executed = rt
        .call_tool("my_tool", "call-2", &json!({ "message": "dyn" }))
        .await
        .expect("execute");
    assert_eq!(executed.content[0].text, "[my_tool] dyn");

    // Unknown command fails gracefully.
    let outcome = rt
        .dispatch_command("does-not-exist", "")
        .await
        .expect("dispatch");
    assert!(!outcome.ok);
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn flag_registration_and_values() {
    let mut rt = new_runtime().await;
    let dir = write_temp_fixture("flags", &[("dynamic-tools.ts", DYNAMIC_TOOLS_TS)]);
    let result = rt
        .load(&[dir.join("dynamic-tools.ts")])
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let flags = rt.flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].name, "echo-uppercase");
    assert_eq!(flags[0].flag_type, "boolean");

    // Default applies before a CLI value is set.
    assert_eq!(rt.get_flag("echo-uppercase"), Some(FlagValue::Bool(false)));
    rt.set_flag_value("echo-uppercase", FlagValue::Bool(true))
        .expect("set");
    assert_eq!(rt.get_flag("echo-uppercase"), Some(FlagValue::Bool(true)));

    // Unregistered flags resolve to None.
    assert_eq!(rt.get_flag("nope"), None);
    assert!(rt.set_flag_value("nope", FlagValue::Bool(true)).is_err());
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn pi_exec_runs_subprocess() {
    let rt = new_runtime().await;
    let exec_call = if cfg!(windows) {
        r#"await pi.exec("cmd", ["/C", "echo", "hello-from-exec"])"#
    } else {
        r#"await pi.exec("sh", ["-c", "echo hello-from-exec"])"#
    };
    let source = r#"
        export default (pi) => {
            pi.on("session_start", async () => {
                const result = __EXEC_CALL__;
                globalThis.__exec_result = result;
            });
        };
        "#
    .replace("__EXEC_CALL__", exec_call);
    rt.seed_module("pir-ext:exec-user.ts", &source)
        .expect("seed");
    rt.seeded_load("pir-ext:exec-user.ts").await.expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let result: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__exec_result)")
        .await
        .expect("eval");
    assert_eq!(result["code"], 0);
    assert_eq!(result["killed"], false);
    // `ExecResult.truncated` is a required field upstream (exec.ts:32-40).
    assert_eq!(result["truncated"], false);
    assert!(result["stdout"]
        .as_str()
        .unwrap_or("")
        .trim()
        .contains("hello-from-exec"));
}

// ---------------------------------------------------------------------------
// Stale-instance protocol
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn invalidate_makes_calls_fail_with_isolated_errors() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:stale.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", () => {
                pi.registerTool({
                    name: "always-new",
                    parameters: { type: "object", properties: {} },
                    execute: async () => ({ content: [], details: null }),
                });
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:stale.ts").await.expect("load");

    // Before invalidation the handler registers a tool.
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    rt.invalidate();

    // After invalidation the captured handler throws; the error is isolated
    // into the event outcome rather than crashing dispatch.
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert_eq!(outcome.errors.len(), 1);
    assert!(
        outcome.errors[0].error.contains("invalidated"),
        "{:?}",
        outcome.errors
    );

    // Tool execution is refused too.
    let err = rt
        .call_tool("always-new", "call-3", &json!({}))
        .await
        .expect_err("tool call must fail after invalidate");
    assert!(err.to_string().contains("invalidated"), "{err}");
}

// ---------------------------------------------------------------------------
// Handler errors are isolated per-handler
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn handler_errors_do_not_break_other_handlers() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:bad-handler.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", () => { throw new Error("handler exploded"); });
            pi.on("session_start", () => { globalThis.__after_bad = true; });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:bad-handler.ts")
        .await
        .expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    println!("DEBUG outcome: {:?}", outcome);
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(outcome.errors[0].error, "handler exploded");

    let after: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__after_bad === true)")
        .await
        .expect("eval");
    assert_eq!(after, json!(true));
}

// ---------------------------------------------------------------------------
// A throwing tool_call handler fails closed (upstream semantics)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn throwing_tool_call_handler_fails_closed() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:failing-gate.ts",
        r#"
        export default (pi) => {
            pi.on("tool_call", () => { throw new Error("gate exploded"); });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:failing-gate.ts")
        .await
        .expect("load");

    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "t1",
        "toolName": "bash",
        "input": { "command": "echo hi" },
    });
    let outcome = rt.emit("tool_call", &mut payload).await.expect("emit");
    assert_eq!(outcome.result["block"], true, "{:?}", outcome.result);
    assert!(
        outcome.errors[0].error.contains("gate exploded"),
        "{:?}",
        outcome.errors
    );
}

// ---------------------------------------------------------------------------
// TypeBox shim surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn typebox_shim_produces_valid_json_schema() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:schema-user.ts",
        r#"
        import { Type } from "typebox";

        export default (pi) => {
                globalThis.__schema = Type.Object({
                required: Type.String({ description: "a required string" }),
                optional: Type.Optional(Type.Number({ description: "an optional number" })),
                tags: Type.Array(Type.String(), { description: "tags" }),
                enabled: Type.Boolean({ default: false }),
                undefinedValue: Type.Undefined(),
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:schema-user.ts")
        .await
        .expect("load");

    let schema: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__schema)")
        .await
        .expect("eval");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["required"]["type"], "string");
    assert_eq!(schema["properties"]["optional"]["type"], "number");
    assert_eq!(schema["properties"]["tags"]["type"], "array");
    assert_eq!(schema["properties"]["enabled"]["default"], false);
    assert_eq!(schema["properties"]["undefinedValue"]["type"], "undefined");
    // Optional properties are excluded from `required`; the shim marker is
    // stripped.
    assert_eq!(
        schema["required"],
        json!(["required", "tags", "enabled", "undefinedValue"])
    );
    assert!(
        serde_json::to_string(&schema).unwrap().is_ascii(),
        "schema must be JSON-serializable"
    );
    let schema_str = serde_json::to_string(&schema).unwrap();
    assert!(!schema_str.contains("__pir_optional__"));
}

// ---------------------------------------------------------------------------
// Messaging notifications
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn send_user_message_and_append_entry_are_captured() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:messaging.ts",
        r#"
        export default (pi) => {
            pi.on("input", () => {
                pi.sendUserMessage("auto follow-up");
                pi.appendEntry("my-custom", { n: 1 });
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:messaging.ts").await.expect("load");

    let mut payload = json!({ "text": "hi", "source": "interactive" });
    let outcome = rt.emit("input", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let notifications = rt.drain_notifications();
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::SendUserMessage { .. })));
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::AppendEntry { .. })));
}

// ---------------------------------------------------------------------------
// Discovery (manifest > index > top-level)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn discovers_manifest_directory() {
    let dir = write_temp_fixture(
        "manifest",
        &[
            (
                "package.json",
                r#"{"pi":{"extensions":["./sub/entry.ts"]}}"#,
            ),
            (
                "sub/entry.ts",
                r#"export default (pi: unknown) => { void pi; };"#,
            ),
            ("index.ts", "export default () => {};"),
        ],
    );

    let rt = new_runtime().await;
    let result = rt.load(&[dir]).await.expect("load");
    assert_eq!(result.loaded.len(), 1, "{:?}", result.errors);
    assert!(result.loaded[0].path.ends_with("entry.ts"));
}

// ---------------------------------------------------------------------------
// Notifications reach the drain queue (the removed HostBridge seam recorded
// side effects through the same path)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn side_effects_reach_the_notification_queue() {
    let rt = ExtensionRuntime::new(test_config()).await.expect("runtime");

    let dir = write_temp_fixture("bridge", &[("dynamic-tools.ts", DYNAMIC_TOOLS_TS)]);
    let result = rt
        .load(&[dir.join("dynamic-tools.ts")])
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let mut payload = json!({ "reason": "startup" });
    rt.emit("session_start", &mut payload).await.expect("emit");
    let notifications = rt.drain_notifications();
    assert!(
        notifications
            .iter()
            .any(|notification| matches!(notification, Notification::Notify { .. })),
        "expected a notify notification, got {notifications:?}"
    );
}

// ---------------------------------------------------------------------------
// Transpile pipeline (moved from src unit tests; contract checks)
// ---------------------------------------------------------------------------

#[test]
fn transpile_strips_types_and_keeps_esm() {
    let src = r#"
        import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
        import { Type } from "typebox";

        interface Opts { message: string }

        export default function (pi: ExtensionAPI): void {
            const opts: Opts = { message: "hi" };
            const t = Type.String({ description: "x" });
            console.log(opts.message, t.type);
        }
    "#;
    let out = pir::extensions::transpile("test.ts", src).expect("transpile should succeed");
    assert!(
        !out.contains("ExtensionAPI"),
        "type-only import must be stripped: {out}"
    );
    assert!(
        out.contains("from \"typebox\"") || out.contains("from 'typebox'"),
        "value import must remain: {out}"
    );
    assert!(
        out.contains("export default"),
        "default export must remain: {out}"
    );
    assert!(
        !out.contains("interface"),
        "interfaces must be stripped: {out}"
    );
}

#[test]
fn transpile_rejects_garbage() {
    assert!(pir::extensions::transpile("bad.ts", "this is (not ] typescript").is_err());
}

#[test]
fn transpile_handles_plain_js() {
    let out = pir::extensions::transpile(
        "plain.js",
        "export default function (pi) { pi.on('x', () => {}); }",
    )
    .expect("plain js transpiles");
    assert!(out.contains("export default"), "{out}");
}

// ---------------------------------------------------------------------------
// Module naming
// ---------------------------------------------------------------------------

#[test]
fn module_names_use_forward_slashes() {
    let name = pir::extensions::loader::module_name_for(&PathBuf::from("C:\\dir\\ext.ts"));
    assert!(name.starts_with("pir-ext:C:/dir/ext.ts"), "{name}");
}

// ---------------------------------------------------------------------------
// Session name roundtrip (setSessionName / getSessionName)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn session_name_roundtrip() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:name.ts",
        r#"
        export default (pi) => {
            globalThis.__name_before = pi.getSessionName();
            pi.setSessionName("renamed-session");
            globalThis.__name_after = pi.getSessionName();
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:name.ts").await.expect("load");

    let before: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__name_before ?? null)")
        .await
        .expect("eval");
    assert_eq!(before, json!("test-session"));

    let after: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__name_after ?? null)")
        .await
        .expect("eval");
    assert_eq!(after, json!("renamed-session"));
}

// ---------------------------------------------------------------------------
// Abort plumbing: host abort is observed by the tool signal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn abort_flag_reaches_tool_signal() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:abort.ts",
        r#"
        export default (pi) => {
            pi.registerTool({
                name: "probe",
                parameters: { type: "object", properties: {} },
                execute: async (id, params, signal) => ({
                    content: [{ type: "text", text: signal.aborted ? "aborted" : "live" }],
                }),
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:abort.ts").await.expect("load");

    let live = rt.call_tool("probe", "c1", &json!({})).await.expect("call");
    assert_eq!(live.content[0].text, "live");

    rt.abort_current_call();
    let aborted = rt.call_tool("probe", "c2", &json!({})).await.expect("call");
    assert_eq!(aborted.content[0].text, "aborted");

    rt.clear_abort().await;
    let again = rt.call_tool("probe", "c3", &json!({})).await.expect("call");
    assert_eq!(again.content[0].text, "live");
}

// ---------------------------------------------------------------------------
// Abort plumbing: ctx.abort() fires in-JS signal listeners
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ctx_abort_fires_signal_listener() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:listener.ts",
        r#"
        export default (pi) => {
            pi.registerTool({
                name: "listen",
                parameters: { type: "object", properties: {} },
                execute: async (id, params, signal, onUpdate, ctx) => {
                    let fired = false;
                    signal.addEventListener("abort", () => { fired = true; });
                    ctx.abort();
                    return { content: [{ type: "text", text: fired ? "fired" : "not-fired" }] };
                },
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:listener.ts").await.expect("load");

    let result = rt
        .call_tool("listen", "c1", &json!({}))
        .await
        .expect("call");
    assert_eq!(result.content[0].text, "fired");

    let notifications = rt.drain_notifications();
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::Abort)));
}

// ---------------------------------------------------------------------------
// withFileMutationQueue: per-path serialization with real return values
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn with_file_mutation_queue_serializes_per_path() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:fmq.ts",
        r#"
        import { withFileMutationQueue } from "@earendil-works/pi-coding-agent";
        export default (pi) => {
            pi.registerTool({
                name: "queue-probe",
                parameters: { type: "object", properties: {} },
                execute: async () => {
                    const order = [];
                    const first = withFileMutationQueue("a.txt", async () => {
                        order.push("first-start");
                        await Promise.resolve();
                        order.push("first-end");
                        return 1;
                    });
                    const second = withFileMutationQueue("a.txt", async () => {
                        order.push("second-start");
                        return 2;
                    });
                    const other = withFileMutationQueue("b.txt", async () => {
                        order.push("other-start");
                        return 3;
                    });
                    const values = await Promise.all([first, second, other]);
                    return {
                        content: [{ type: "text", text: JSON.stringify({ order, values }) }],
                    };
                },
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:fmq.ts").await.expect("load");

    let result = rt
        .call_tool("queue-probe", "q1", &json!({}))
        .await
        .expect("call");
    let parsed: serde_json::Value = serde_json::from_str(&result.content[0].text).expect("json");
    assert_eq!(parsed["values"], json!([1, 2, 3]));
    let order = parsed["order"].as_array().expect("order array").clone();
    let first_end = order
        .iter()
        .position(|value| value == "first-end")
        .expect("first-end present");
    let second_start = order
        .iter()
        .position(|value| value == "second-start")
        .expect("second-start present");
    assert!(
        first_end < second_start,
        "same-path operations must serialize: {order:?}"
    );
}

// ---------------------------------------------------------------------------
// pi.exec output bounding
// ---------------------------------------------------------------------------

/// `c466a3595` #11546: `maxBuffer` caps combined stdout+stderr, terminates
/// the command, and reports `truncated: true` with a nonzero exit code
/// (upstream `exec.ts:90-102,130-140`). The output carries no textual notice.
#[tokio::test(flavor = "current_thread")]
async fn exec_output_respects_max_buffer_and_reports_truncation() {
    let rt = new_runtime().await;
    let exec_call = if cfg!(windows) {
        r#"await pi.exec("cmd", [
                    "/C",
                    `for /L %i in (1,1,3000) do @echo ${line}`,
                ], { maxBuffer: 1024 })"#
    } else {
        r#"await pi.exec("sh", ["-c", `yes ${line} | head -n 3000`], { maxBuffer: 1024 })"#
    };
    let source = r#"
        export default (pi) => {
            pi.on("session_start", async () => {
                // The literal must sit directly in the loop body: cmd expands
                // %VAR% at parse time, so `set L=... & echo %L%` on one line
                // would echo an unexpanded token.
                const line =
                    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789" +
                    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789" +
                    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789" +
                    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789" +
                    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789";
                const result = __EXEC_CALL__;
                globalThis.__exec_big = result;
            });
        };
        "#
    .replace("__EXEC_CALL__", exec_call);
    rt.seed_module("pir-ext:bigout.ts", &source).expect("seed");
    rt.seeded_load("pir-ext:bigout.ts").await.expect("load");

    let mut payload = json!({ "reason": "startup" });
    rt.emit("session_start", &mut payload).await.expect("emit");

    let result: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__exec_big)")
        .await
        .expect("eval");
    let stdout = result["stdout"].as_str().unwrap_or("");
    let stderr = result["stderr"].as_str().unwrap_or("");
    assert_eq!(
        result["truncated"], true,
        "truncation is reported: {result}"
    );
    assert_eq!(
        result["killed"], true,
        "the command was terminated: {result}"
    );
    assert_ne!(
        result["code"], 0,
        "a truncated result is not success: {result}"
    );
    assert!(
        stdout.len() + stderr.len() <= 1024,
        "combined output must stay within maxBuffer: {} bytes",
        stdout.len() + stderr.len()
    );
    assert!(
        !stdout.contains("[output truncated]") && !stderr.contains("[output truncated]"),
        "upstream returns incomplete output without a notice"
    );
}

/// `maxBuffer: 0` truncates every byte; the command is still terminated and
/// the result never reads as success (upstream `exec.test.ts`, #11360).
#[tokio::test(flavor = "current_thread")]
async fn exec_zero_max_buffer_truncates_all_output() {
    let rt = new_runtime().await;
    let exec_call = if cfg!(windows) {
        r#"await pi.exec("cmd", ["/C", "echo", "x"], { maxBuffer: 0 })"#
    } else {
        r#"await pi.exec("sh", ["-c", "printf x"], { maxBuffer: 0 })"#
    };
    let source = r#"
        export default (pi) => {
            pi.on("session_start", async () => {
                globalThis.__exec_zero = __EXEC_CALL__;
            });
        };
        "#
    .replace("__EXEC_CALL__", exec_call);
    rt.seed_module("pir-ext:zero-buffer.ts", &source)
        .expect("seed");
    rt.seeded_load("pir-ext:zero-buffer.ts")
        .await
        .expect("load");

    let mut payload = json!({ "reason": "startup" });
    rt.emit("session_start", &mut payload).await.expect("emit");

    let result: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__exec_zero)")
        .await
        .expect("eval");
    assert_eq!(result["stdout"], "");
    assert_eq!(result["stderr"], "");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["killed"], true);
    assert_ne!(result["code"], 0);
}

/// An invalid `maxBuffer` rejects with `RangeError` before the command is
/// spawned; a missing command would otherwise resolve to a `code: 1` result
/// (upstream `exec.ts:53-57`, `exec.test.ts`).
#[tokio::test(flavor = "current_thread")]
async fn exec_rejects_invalid_max_buffer_before_spawning() {
    let rt = new_runtime().await;
    let source = r#"
        export default (pi) => {
            pi.on("session_start", async () => {
                const outcomes = [];
                for (const maxBuffer of [-1, 0.5, Number.NaN, Number.POSITIVE_INFINITY]) {
                    try {
                        await pi.exec("pir-command-that-does-not-exist-11360", [], { maxBuffer });
                        outcomes.push("no-error");
                    } catch (error) {
                        outcomes.push(error.name + ": " + error.message);
                    }
                }
                globalThis.__exec_invalid = outcomes;
            });
        };
        "#;
    rt.seed_module("pir-ext:invalid-buffer.ts", source)
        .expect("seed");
    rt.seeded_load("pir-ext:invalid-buffer.ts")
        .await
        .expect("load");

    let mut payload = json!({ "reason": "startup" });
    rt.emit("session_start", &mut payload).await.expect("emit");

    let outcomes: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__exec_invalid)")
        .await
        .expect("eval");
    let outcomes = outcomes.as_array().expect("outcomes array");
    assert_eq!(outcomes.len(), 4);
    for outcome in outcomes {
        let outcome = outcome.as_str().unwrap_or("");
        assert!(
            outcome.starts_with("RangeError: maxBuffer must be an integer between 0 and"),
            "expected a RangeError from option validation, got {outcome:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// getAgentDir resolves through the host
// ---------------------------------------------------------------------------

/// Fully explicit agent-dir environment for ONE test: CLEARS every override,
/// applies `pins`, and restores every touched variable (absence included) on
/// drop, panic included.
///
/// `getAgentDir()` resolves the process environment on every call, so a host
/// that exports `PIR_AGENT_DIR` (the release-harness isolation variable) or a
/// sibling suite that pins one would otherwise decide what these tests
/// observe. Stating the environment here makes each test deterministic on CI
/// and on any developer machine; no mutation outlives the test.
///
/// These two probes assert HOW THE JS SURFACE RESOLVES THE PROCESS
/// ENVIRONMENT — the ambient value is the property under test, and the JS
/// `getAgentDir()` binding takes no argument that could carry a directory — so
/// they cannot be rewritten around an explicit agent dir. They therefore run
/// only under a process-per-test runner (nextest in CI and the pre-push gate)
/// or when the operator opts in. The gate is therefore the whole mechanism:
/// there is no lock, because a lock can only serialize this file against
/// itself while leaving every other suite in the binary free to re-point the
/// variable (TEST-2 in `.audit/review-2026-11-23/findings.jsonl`).
/// Clear the agent-dir overrides plus every name in `pins`, set `pins`, and
/// restore everything on drop. Returns `None` when the process is shared with
/// other suites and the operator has not opted in (the caller then skips).
fn agent_dir_env(pins: &[(&'static str, &str)]) -> Option<crate::common::ProcessEnvPin> {
    if crate::common::skip_without_nextest("extensions_runtime::getAgentDir probe") {
        return None;
    }
    Some(crate::common::ProcessEnvPin::isolate(
        &crate::common::AGENT_DIR_ENV_VARS,
        pins,
    ))
}

/// The default agent dir is `<home>/.pi/agent`. The test pins `HOME` (the
/// primary fallback input) and `USERPROFILE` (the Windows fallback) at a
/// scratch directory and clears every override, so the assertion is EXACT
/// instead of a substring guess a host env could satisfy by accident. No
/// mutation outlives the test.
#[tokio::test(flavor = "current_thread")]
async fn get_agent_dir_resolves_to_host_path() {
    let home = std::env::temp_dir().join(format!(
        "pir-ext-agent-dir-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create scratch home");
    let home_str = home.to_str().expect("temp path is UTF-8");

    let Some(_env) = agent_dir_env(&[("HOME", home_str), ("USERPROFILE", home_str)]) else {
        let _ = std::fs::remove_dir_all(&home);
        return;
    };

    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:agentdir.ts",
        r#"
        import { getAgentDir } from "@earendil-works/pi-coding-agent";
        export default () => {
            globalThis.__agent_dir = getAgentDir();
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:agentdir.ts").await.expect("load");

    let dir: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__agent_dir ?? null)")
        .await
        .expect("eval");
    let dir = dir.as_str().unwrap_or("");
    let expected = home.join(".pi").join("agent");
    assert_eq!(
        PathBuf::from(dir.replace('\\', "/")),
        PathBuf::from(expected.to_string_lossy().replace('\\', "/")),
        "with no override set, getAgentDir must resolve <HOME>/.pi/agent"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// `getAgentDir()` from extension context must honor `PIR_AGENT_DIR` alone —
/// the same alias the session dir and the TUI themes dir honor (upstream
/// parity: one agent-dir resolution across every host surface).
#[tokio::test(flavor = "current_thread")]
async fn get_agent_dir_honors_pir_agent_dir_alias() {
    let dir = std::env::temp_dir().join(format!(
        "pir-ext-agent-dir-alias-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create agent dir");

    let Some(_env) = agent_dir_env(&[]) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    std::env::set_var("PIR_AGENT_DIR", &dir);

    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:agentdir-alias.ts",
        r#"
        import { getAgentDir } from "@earendil-works/pi-coding-agent";
        export default () => {
            globalThis.__agent_dir = getAgentDir();
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:agentdir-alias.ts")
        .await
        .expect("load");

    let resolved: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__agent_dir ?? null)")
        .await
        .expect("eval");
    let resolved = resolved.as_str().unwrap_or("");
    assert_eq!(
        PathBuf::from(resolved.replace('\\', "/")),
        PathBuf::from(dir.to_string_lossy().replace('\\', "/")),
        "getAgentDir must resolve under PIR_AGENT_DIR, got {resolved}"
    );

    // `_env` restores PIR_AGENT_DIR (absence included) on drop.
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// ctx.ui TUI-facing methods (setTitle / setWorkingMessage / setWidget)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ctx_ui_set_title_and_working_message_are_captured() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:uititle.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (_event, ctx) => {
                ctx.ui.setTitle("pir · session");
                ctx.ui.setWorkingMessage("Thinking hard …");
                ctx.ui.setWorkingVisible(false);
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:uititle.ts").await.expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let notifications = rt.drain_notifications();
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::SetTitle(title) if title == "pir · session")));
    assert!(notifications.iter().any(|n| matches!(
        n,
        Notification::WorkingMessage(message) if message == "Thinking hard …"
    )));
    // `setWorkingVisible(false)` rides its own notification channel: an
    // empty working message would read as "streaming with the default label"
    // to the TUI, so a hide request is never encoded there.
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::WorkingVisible(false))));
}

#[tokio::test(flavor = "current_thread")]
async fn ctx_ui_set_widget_add_and_remove() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:uiwidget.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (_event, ctx) => {
                ctx.ui.setWidget("clock", { text: "12:00", placement: "footer" });
                ctx.ui.setWidget("badge", { text: "badge", placement: "unknown-mode" });
                ctx.ui.setWidget("pane", { text: "pane", placement: "belowEditor" });
                ctx.ui.setWidget("cleared", null);
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:uiwidget.ts").await.expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let notifications = rt.drain_notifications();
    let clock = notifications.iter().find_map(|n| match n {
        Notification::Widget { id, content } if id == "clock" => Some(content.clone()),
        _ => None,
    });
    let clock = clock.expect("clock widget notification");
    assert_eq!(clock["text"], "12:00");
    // Placement strings are normalized host-side before the notification
    // leaves the runtime (`prelude.rs`): only the below-editor variants
    // (`belowEditor` / `below_transcript`) fold to `below_transcript`;
    // anything else — including the legacy "footer" — folds to
    // `above_transcript`.
    assert_eq!(clock["placement"], "above_transcript");

    // Unknown placement strings do not survive verbatim either; they fold to
    // the above-transcript default.
    let badge = notifications.iter().find_map(|n| match n {
        Notification::Widget { id, content } if id == "badge" => Some(content.clone()),
        _ => None,
    });
    let badge = badge.expect("badge widget notification");
    assert_eq!(badge["placement"], "above_transcript");

    // Upstream `aboveEditor`/`belowEditor` (types.ts:122): the below-editor
    // variant addresses the below-transcript widget target.
    let pane = notifications.iter().find_map(|n| match n {
        Notification::Widget { id, content } if id == "pane" => Some(content.clone()),
        _ => None,
    });
    let pane = pane.expect("pane widget notification");
    assert_eq!(pane["placement"], "below_transcript");

    // `setWidget(id, null)` produces a null-content notification, which is
    // the documented "remove" signal.
    let cleared = notifications.iter().find_map(|n| match n {
        Notification::Widget { id, content } if id == "cleared" => Some(content.is_null()),
        _ => None,
    });
    assert_eq!(cleared, Some(true), "expected a null-content removal");
}

#[tokio::test(flavor = "current_thread")]
async fn ctx_ui_set_footer_header_indicator_and_thinking_label_are_captured() {
    let rt = new_runtime().await;
    rt.seed_module(
        "pir-ext:uibars.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (_event, ctx) => {
                // Before any registration `getEditorComponent()` is undefined
                // (upstream: undefined while using the default editor).
                ctx.ui.notify("editor:" + typeof ctx.ui.getEditorComponent(), "info");
                ctx.ui.setFooter(() => ({ render: () => ["footer"] }));
                ctx.ui.setHeader(() => ({ render: () => ["header"] }));
                ctx.ui.setWorkingIndicator({ frames: ["*"], intervalMs: 250 });
                ctx.ui.setWorkingIndicator();
                ctx.ui.setHiddenThinkingLabel("Hmm...");
                ctx.ui.setHiddenThinkingLabel();
                // The factory is recorded (getEditorComponent returns it) but
                // never rendered (pir keeps its Rust editor).
                ctx.ui.setEditorComponent((tui, theme, keybindings) => ({ render: () => [] }));
                ctx.ui.notify("editor:" + typeof ctx.ui.getEditorComponent(), "info");
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:uibars.ts").await.expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let notifications = rt.drain_notifications();
    // `setFooter(factory)` / `setHeader(factory)`: the registration signals
    // the TUI renders the factory through the `footerRenderers` /
    // `headerRenderers` round-trip (upstream replaces the built-in bars).
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::Footer(true))));
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::Header(true))));
    // `setWorkingIndicator(options)` carries the prelude-normalized
    // `{frames, intervalMs}`; `setWorkingIndicator()` restores the default
    // (a Null notification).
    assert!(notifications.iter().any(|n| matches!(
        n,
        Notification::WorkingIndicator(options)
            if options["frames"][0] == "*" && options["intervalMs"] == 250
    )));
    assert!(notifications.iter().any(|n| matches!(
        n,
        Notification::WorkingIndicator(options) if options.is_null()
    )));
    // `setHiddenThinkingLabel(label)` / `setHiddenThinkingLabel()` (None
    // restores the default label).
    assert!(notifications.iter().any(|n| matches!(
        n,
        Notification::HiddenThinkingLabel(Some(label)) if label == "Hmm..."
    )));
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::HiddenThinkingLabel(None))));
    // `getEditorComponent()` bookkeeping: undefined -> the registered factory.
    let messages: Vec<String> = notifications
        .iter()
        .filter_map(|n| match n {
            Notification::Notify { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert!(
        messages.iter().any(|message| message == "editor:undefined"),
        "no editor factory before setEditorComponent: {messages:?}"
    );
    assert!(
        messages.iter().any(|message| message == "editor:function"),
        "getEditorComponent returns the registered factory: {messages:?}"
    );
}

// ---------------------------------------------------------------------------
// ctx.sessionManager tree facades (nested `SessionTreeNode` contract)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ctx_session_manager_tree_facades_walk_nested_nodes() {
    let rt = new_runtime().await;
    // The host snapshot is stubbed to the upstream nested `SessionTreeNode`
    // shape (`{entry, children, label, labelTimestamp}`, session-manager.ts:
    // 199-207) — the shape `SessionManager::tree()` now pushes for the
    // extension surface (session_ctx.rs), same as the RPC `get_tree`.
    rt.seed_module(
        "pir-ext:sessiontree.ts",
        r#"
        export default (pi) => {
            globalThis.__pir_host.getSessionManagerJson = () => JSON.stringify({
                tree: [
                    {
                        entry: { type: "message", id: "root-1" },
                        children: [
                            {
                                entry: { type: "message", id: "child-1" },
                                children: [],
                                label: null,
                                id: "child-1",
                            },
                        ],
                        label: "pinned",
                        id: "root-1",
                    },
                ],
                leafId: "child-1",
            });
            pi.on("session_start", (_event, ctx) => {
                const sm = ctx.sessionManager;
                const tree = sm.getTree();
                const leaf = sm.getLeafEntry();
                globalThis.__tree_probe = {
                    roots: tree.length,
                    rootLabel: tree[0]?.label,
                    childId: tree[0]?.children?.[0]?.id,
                    childType: tree[0]?.children?.[0]?.entry?.type,
                    leafId: leaf?.id,
                    leafType: leaf?.type,
                    entryId: sm.getEntry("child-1")?.id,
                    entryType: sm.getEntry("child-1")?.type,
                    missing: sm.getEntry("nope") === undefined,
                };
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:sessiontree.ts")
        .await
        .expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let probe: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__tree_probe)")
        .await
        .expect("eval");
    // getTree() answers the nested forest verbatim (label included).
    assert_eq!(probe["roots"], 1, "{probe}");
    assert_eq!(probe["rootLabel"], "pinned", "{probe}");
    assert_eq!(probe["childId"], "child-1", "{probe}");
    assert_eq!(probe["childType"], "message", "{probe}");
    // getLeafEntry()/getEntry() resolve entries across the whole forest and
    // unwrap the node's `entry` (a NESTED child here, not just a root).
    assert_eq!(probe["leafId"], "child-1", "{probe}");
    assert_eq!(probe["leafType"], "message", "{probe}");
    assert_eq!(probe["entryId"], "child-1", "{probe}");
    assert_eq!(probe["entryType"], "message", "{probe}");
    assert_eq!(probe["missing"], true, "{probe}");
}

/// The `.js/.mjs/.cjs` fast path: these modules are handed to QuickJS
/// byte-identically and are NOT parsed by SWC (the old content probe parsed
/// every module just to conclude "plain JS"; that transient AST was the
/// extension-load memory high-water mark). TypeScript syntax inside a `.js`
/// file is therefore a runtime syntax error (Node parity), not silently
/// stripped — this pins that contract.
#[test]
fn js_modules_are_not_transpiled_even_with_ts_syntax() {
    let src = "// keep the comment and spacing\nconst x: number = 1;\n\nexport default x;\n";
    let out = pir::extensions::transpile("module.js", src).expect("the fast path never fails");
    assert_eq!(
        out, src,
        "a .js module must reach the engine verbatim (no SWC reprint, no TS stripping)"
    );

    let mjs = "export const y = 2;\n";
    assert_eq!(pir::extensions::transpile("module.mjs", mjs).unwrap(), mjs);
    let cjs = "module.exports = { z: 3 };\n";
    assert_eq!(pir::extensions::transpile("module.cjs", cjs).unwrap(), cjs);
}

// ---------------------------------------------------------------------------
// Disk-module bytecode cache
// ---------------------------------------------------------------------------

/// The key must be a function of the module name and the raw source only: a
/// stable key lets a later `pir` start reuse the blob, and any edit or rename
/// must produce a different key.
#[test]
fn bytecode_cache_keys_are_content_and_name_specific() {
    use pir::extensions::bytecode_cache;
    let base = bytecode_cache::key("pir-ext:/tmp/a.ts", b"export default 1;\n");
    assert_eq!(
        base,
        bytecode_cache::key("pir-ext:/tmp/a.ts", b"export default 1;\n"),
        "the same name and source must map to the same key"
    );
    assert_ne!(
        base,
        bytecode_cache::key("pir-ext:/tmp/a.ts", b"export default 2;\n"),
        "an edited source must miss"
    );
    assert_ne!(
        base,
        bytecode_cache::key("pir-ext:/tmp/b.ts", b"export default 1;\n"),
        "a renamed module must miss (bytecode is compiled per module name)"
    );
    assert_eq!(base.len(), 64);
    assert!(base.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

/// `put` → `get` → `evict` round trip. The key is unique per test process so a
/// developer's warm cache can never make this pass without going through the
/// store.
#[test]
fn bytecode_cache_round_trips_and_evicts() {
    use pir::extensions::bytecode_cache;
    let unique = format!(
        "pir-ext:bytecode-cache-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let key = bytecode_cache::key(&unique, b"export default 0;\n");
    bytecode_cache::evict(&key);
    assert!(bytecode_cache::get(&key).is_none(), "a fresh key is empty");

    let payload: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
    bytecode_cache::put(&key, &payload);
    assert_eq!(
        bytecode_cache::get(&key).as_deref(),
        Some(&payload),
        "the stored payload comes back byte-identically"
    );

    bytecode_cache::evict(&key);
    assert!(bytecode_cache::get(&key).is_none(), "evict drops the entry");
}

/// A disk extension loaded through a fresh runtime twice (the second runtime
/// models a worker/reload within one process, and the persisted blob models
/// the next `pir` start) must load, run, and leave a compiled blob behind.
#[tokio::test(flavor = "current_thread")]
async fn disk_extension_loads_and_populates_the_bytecode_cache() {
    use pir::extensions::{bytecode_cache, loader::module_name_for};

    let entry_src = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { helper } from "./helper.ts";

export default function (pi: ExtensionAPI) {
  pi.on("session_start", () => {
    globalThis.__bytecode_cache_probe = helper(20);
  });
}
"#;
    let helper_src = r#"
export function helper(value: number): number {
  const parts: string[] = [];
  for (let i = 0; i < 4; i++) parts.push(`p-${value + i}`);
  return value + parts.length;
}
"#;
    let dir = write_temp_fixture(
        "bytecode-cache",
        &[("index.ts", entry_src), ("helper.ts", helper_src)],
    );
    let entry = dir.join("index.ts");
    let name = module_name_for(&entry);
    let key = bytecode_cache::key(&name, entry_src.as_bytes());
    // Cold start for this exact source revision.
    bytecode_cache::evict(&key);

    let first = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    first.allow_root(dir.clone());
    let outcome = first.load(&[entry.clone()]).await.expect("first load");
    assert_eq!(outcome.loaded.len(), 1, "{:?}", outcome.errors);
    let mut payload = json!({ "reason": "startup" });
    first
        .emit("session_start", &mut payload)
        .await
        .expect("first emit");
    assert!(
        bytecode_cache::get(&key).is_some(),
        "the entry blob is stored"
    );

    // Second runtime: the same modules must load again (memory/disk hit or a
    // transparent miss-and-recompile) and still execute the imported helper.
    let second = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    second.allow_root(dir.clone());
    let outcome = second.load(&[entry]).await.expect("second load");
    assert_eq!(outcome.loaded.len(), 1, "{:?}", outcome.errors);
    second
        .emit("session_start", &mut payload)
        .await
        .expect("second emit");
    let probe: serde_json::Value = second
        .eval_json("JSON.stringify(globalThis.__bytecode_cache_probe)")
        .await
        .expect("eval");
    assert_eq!(probe, json!(24), "the module ran through helper.ts");

    bytecode_cache::evict(&key);
}

// ---------------------------------------------------------------------------
// AOT boot-path pin
// ---------------------------------------------------------------------------

/// The same probe runs in-process (AOT available) and in a child with
/// `PIR_EXT_BYTECODE=0` (source path), so both boots must agree. It covers the
/// three things the AOT'd bundles install: the builtin sweep (`require("fs")`
/// works synchronously), `process.platform` (dynamic constants eval) and the
/// global `Buffer`/TextEncoder surface (static globals bundle).
async fn boot_probe(runtime: &ExtensionRuntime) -> serde_json::Value {
    runtime
        .eval_json(
            r#"JSON.stringify((function () {
              const fs = require("fs");
              return {
                platform: process.platform,
                platformGlobal: globalThis.__pirPlatform,
                fsRead: typeof fs.readFileSync,
                fsSync: typeof fs.statSync,
                buffer: typeof globalThis.Buffer.from,
                textEncoder: typeof TextEncoder,
                requireFn: typeof require,
              };
            })())"#,
        )
        .await
        .expect("boot probe")
}

fn expected_boot_probe() -> serde_json::Value {
    json!({
        "platform": pir::extensions::api::node_shims::NODE_PLATFORM,
        "platformGlobal": pir::extensions::api::node_shims::NODE_PLATFORM,
        "fsRead": "function",
        "fsSync": "function",
        "buffer": "function",
        "textEncoder": "function",
        "requireFn": "function",
    })
}

/// Child-process body: inherited by the parent test with `PIR_EXT_BYTECODE=0`
/// and the skip toggles cleared, so it exercises the source fallback for every
/// boot shim.
#[tokio::test(flavor = "current_thread")]
#[ignore = "spawned by boot_bundles_are_aot_compiled_and_toggle_boots_identically"]
async fn boot_bundle_probe_child() {
    let expected_enabled = !matches!(
        std::env::var("PIR_EXT_BYTECODE").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    );
    assert_eq!(
        pir::extensions::api::shims::bytecode_enabled(),
        expected_enabled,
        "the child must boot with the requested bytecode mode"
    );
    let runtime = new_runtime().await;
    assert_eq!(boot_probe(&runtime).await, expected_boot_probe());
}

/// Pins the boot bundles to the AOT registry and proves the `PIR_EXT_BYTECODE=0`
/// escape hatch still boots them from source with identical installed globals.
#[tokio::test(flavor = "current_thread")]
async fn boot_bundles_are_aot_compiled_and_toggle_boots_identically() {
    use pir::extensions::api::{bytecode, node_shims};

    // Registry pin: both assembled bundles must have a guard-valid blob.
    for name in [
        node_shims::GLOBALS_BUNDLE,
        node_shims::NODE_MODULES_BOOTSTRAP_BUNDLE,
    ] {
        let blob = bytecode::bytecode_for(name)
            .unwrap_or_else(|| panic!("{name} must be embedded as AOT bytecode"));
        assert!(
            bytecode::payload(blob).is_some(),
            "{name} guard header must validate (regenerate with pir-gen-bytecode)"
        );
    }

    let before = bytecode::aot_loads();
    let runtime = new_runtime().await;
    let probed = boot_probe(&runtime).await;
    assert_eq!(probed, expected_boot_probe());

    if pir::extensions::api::shims::bytecode_enabled() {
        // prelude + globals + builtin bootstrap all take the AOT path.
        assert!(
            bytecode::aot_loads() >= before + 3,
            "a boot must evaluate the prelude, globals and builtin bootstrap from bytecode \
             (aot_loads: before={before} after={})",
            bytecode::aot_loads()
        );
        // The emitted globals blob is the assembled bundle minus the dynamic
        // constants: pin that it is non-trivial and covers the whole text.
        let globals_payload = bytecode::payload(
            bytecode::bytecode_for(node_shims::GLOBALS_BUNDLE).expect("globals blob"),
        )
        .expect("globals payload");
        assert!(
            globals_payload.len() > 64 * 1024,
            "globals bundle bytecode should be substantial, got {} bytes",
            globals_payload.len()
        );
    }

    // Force the source path in a fresh process (the mode is cached per
    // process, so it must be a child) and compare the exact probe.
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "boot_bundle_probe_child",
            "--include-ignored",
            "--nocapture",
        ])
        .env("PIR_EXT_BYTECODE", "0")
        .env_remove("PIR_EXT_SKIP_PRELUDE")
        .env_remove("PIR_EXT_SKIP_NODE_BUILTINS")
        .status()
        .expect("spawn source-path boot probe");
    assert!(status.success(), "source-path boot probe failed: {status}");
}

// ---------------------------------------------------------------------------
// Boot-cost measurement (manual, `--ignored`)
// ---------------------------------------------------------------------------

/// Mean warm `ExtensionRuntime` construction cost, with the boot-stage skip
/// toggles to attribute the source-parse work. Run alone:
///
/// ```text
/// cargo test --features extensions --test <group> \
///   extensions_runtime::boot_cost_measurement -- --include-ignored --nocapture
/// ```
#[tokio::test(flavor = "current_thread")]
#[ignore = "manual boot-cost measurement"]
async fn boot_cost_measurement() {
    use pir::extensions::api::node_shims;
    use std::time::{Duration, Instant};

    const WARMUP: usize = 3;
    const ITERS: usize = 20;

    println!(
        "bundle bytes: globals={} bootstrap={} prelude={} builtins={}",
        node_shims::globals_source().len(),
        node_shims::node_modules_bootstrap_source().len(),
        pir::extensions::api::HOST_PRELUDE.source().len(),
        node_shims::builtin_short_names().len(),
    );

    // Direct parse+eval cost of the globals bundle in a bare engine (no host
    // registration, no sweep): upper bound for what AOT can remove.
    {
        let rt = rquickjs::Runtime::new().expect("quickjs runtime");
        let ctx = rquickjs::Context::full(&rt).expect("quickjs context");
        for _ in 0..WARMUP {
            ctx.with(|ctx| {
                ctx.eval::<(), _>(node_shims::globals_source())
                    .expect("globals eval");
            });
        }
        let mut samples = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let started = Instant::now();
            ctx.with(|ctx| {
                ctx.eval::<(), _>(node_shims::globals_source())
                    .expect("globals eval");
            });
            samples.push(started.elapsed());
        }
        println!(
            "raw globals_source eval: mean={:.3}ms min={:.3}ms",
            mean(&samples).as_secs_f64() * 1e3,
            samples.iter().min().copied().unwrap().as_secs_f64() * 1e3,
        );
    }

    let scenarios: &[(&str, bool, bool)] = &[
        ("full", false, false),
        ("skip prelude", true, false),
        ("skip builtins", false, true),
        ("skip prelude+builtins", true, true),
    ];
    for (label, skip_prelude, skip_builtins) in scenarios {
        if *skip_prelude {
            std::env::set_var("PIR_EXT_SKIP_PRELUDE", "1");
        } else {
            std::env::remove_var("PIR_EXT_SKIP_PRELUDE");
        }
        if *skip_builtins {
            std::env::set_var("PIR_EXT_SKIP_NODE_BUILTINS", "1");
        } else {
            std::env::remove_var("PIR_EXT_SKIP_NODE_BUILTINS");
        }
        for _ in 0..WARMUP {
            let runtime = ExtensionRuntime::new(test_config())
                .await
                .expect("runtime construction");
            drop(runtime);
        }
        let mut samples = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let started = Instant::now();
            let runtime = ExtensionRuntime::new(test_config())
                .await
                .expect("runtime construction");
            samples.push(started.elapsed());
            drop(runtime);
        }
        println!(
            "{label}: mean={:.3}ms min={:.3}ms",
            mean(&samples).as_secs_f64() * 1e3,
            samples.iter().min().copied().unwrap().as_secs_f64() * 1e3,
        );
    }
    std::env::remove_var("PIR_EXT_SKIP_PRELUDE");
    std::env::remove_var("PIR_EXT_SKIP_NODE_BUILTINS");

    fn mean(samples: &[Duration]) -> Duration {
        let total: Duration = samples.iter().sum();
        total / u32::try_from(samples.len()).expect("sample count fits u32")
    }
}
