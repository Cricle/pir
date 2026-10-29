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

// ---------------------------------------------------------------------------
// Spike-level smoke: TS string → transpile → QuickJS → host `pi` object
// ---------------------------------------------------------------------------

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
    rt.emit("session_start", &mut payload).await.expect("emit");

    let result: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__exec_result)")
        .await
        .expect("eval");
    assert_eq!(result["code"], 0);
    assert_eq!(result["killed"], false);
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
    // Optional properties are excluded from `required`; the shim marker is
    // stripped.
    assert_eq!(schema["required"], json!(["required", "tags", "enabled"]));
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

#[tokio::test(flavor = "current_thread")]
async fn exec_output_is_bounded() {
    let rt = new_runtime().await;
    let exec_call = if cfg!(windows) {
        r#"await pi.exec("cmd", [
                    "/C",
                    `for /L %i in (1,1,3000) do @echo ${line}`,
                ])"#
    } else {
        r#"await pi.exec("sh", ["-c", `yes ${line} | head -n 3000`])"#
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
    assert!(
        stdout.contains("[output truncated]"),
        "expected truncation notice, got {} bytes",
        stdout.len()
    );
    assert!(
        stdout.len() < 2 * 1024 * 1024,
        "output must stay bounded: {} bytes",
        stdout.len()
    );
}

// ---------------------------------------------------------------------------
// getAgentDir resolves through the host
// ---------------------------------------------------------------------------

/// Serializes every test that mutates the agent-dir env vars (process-global).
static AGENT_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn get_agent_dir_resolves_to_host_path() {
    let _guard = AGENT_DIR_ENV_LOCK.lock().unwrap();
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
    assert!(
        dir.contains(".pi"),
        "expected ~/.pi/agent style path, got {dir}"
    );
    assert!(
        dir.replace('\\', "/").ends_with(".pi/agent"),
        "expected path to end with .pi/agent, got {dir}"
    );
}

/// `getAgentDir()` from extension context must honor `PIR_AGENT_DIR` alone —
/// the same alias the session dir and the TUI themes dir honor (upstream
/// parity: one agent-dir resolution across every host surface).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn get_agent_dir_honors_pir_agent_dir_alias() {
    let _guard = AGENT_DIR_ENV_LOCK.lock().unwrap();
    let previous = std::env::var("PIR_AGENT_DIR").ok();
    let dir = std::env::temp_dir().join("pir-ext-agent-dir-alias");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create agent dir");
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

    match previous.as_deref() {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
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
