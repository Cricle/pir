//! Parity tests for the two `ctx.ui` JS-extension gaps from
//! `logs/perf/agent-parity-audit.md` §1.5:
//!
//! 1. `getToolsExpanded` / `setToolsExpanded` share ONE
//!    `Arc<AtomicBool>` cell with the TUI (the same value the transcript's
//!    tool blocks render from and Ctrl+O writes), instead of the old
//!    JS-local mirror.
//! 2. Dialogs emit `ui_prompt_start` before awaiting and
//!    `ui_prompt_end` exactly once on every outcome (ok / cancel / error /
//!    timeout), with pi's payload fields
//!    (`{type, reason, kind, title?}` — upstream types.ts:1039-1055,
//!    runner.ts:577-616).
//!
//! The native `UiService` dialog path is not exercised: its dialog methods
//! fail closed (`plugin_services.rs` is owned by another lane and has no
//! native reply seam yet), so only the JS host-function path exists today.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-ctx-ui-parity-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn handle_config(cwd: &Path, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn started_handle(test_name: &str, has_ui: bool) -> (ExtensionHandle, PathBuf) {
    let dir = fixture_dir(test_name);
    let handle = ExtensionHandle::start(handle_config(&dir, has_ui)).expect("runtime start");
    (handle, dir)
}

async fn seed_and_load(handle: &ExtensionHandle, dir: &Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let path = dir.join(name);
    let result = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

fn tool_text(result: &pir::extensions::ToolResult) -> String {
    result
        .content
        .first()
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

/// Pull the first pending dialog request (the TUI drain's observation point;
/// a test answers it through the same `respond_dialog` reply path).
async fn take_dialog(handle: &ExtensionHandle) -> (u64, String, Value) {
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let requests = handle.take_notifications(|notification| {
            matches!(notification, Notification::DialogRequest { .. })
        });
        if let Some(Notification::DialogRequest {
            id,
            kind,
            title,
            options,
        }) = requests.into_iter().last()
        {
            return (id, kind, json!({ "title": title, "options": options }));
        }
    }
    panic!("dialog request was not observed");
}

// ---------------------------------------------------------------------------
// Gap 1: one shared tools-expanded cell
// ---------------------------------------------------------------------------

const TOOLS_EXPANDED_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "expanded_probe",
    description: "reads and optionally writes ctx.ui.getToolsExpanded",
    parameters: {
      type: "object",
      properties: { value: { type: "boolean" } },
    },
    async execute(_id, params, _signal, _onUpdate, ctx) {
      const before = ctx.ui.getToolsExpanded();
      let after = null;
      if (params && params.value !== undefined) {
        ctx.ui.setToolsExpanded(params.value);
        after = ctx.ui.getToolsExpanded();
      }
      return { content: [{ type: "text", text: JSON.stringify({ before, after }) }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn tools_expanded_is_one_shared_cell() {
    let (handle, dir) = started_handle("tools-expanded", false).await;
    seed_and_load(&handle, &dir, "expanded.ts", TOOLS_EXPANDED_TS).await;

    // Default: collapsed, in the extension AND in the shared TUI cell.
    assert!(!handle.tools_expanded(), "default is collapsed");
    assert!(
        !handle
            .tools_expanded_cell()
            .load(std::sync::atomic::Ordering::SeqCst),
        "the shared cell starts collapsed"
    );

    // Extension -> shared cell.
    let result = handle
        .call_tool("expanded_probe", "c1", &json!({ "value": true }))
        .await
        .expect("probe call");
    assert_eq!(tool_text(&result), r#"{"before":false,"after":true}"#);
    assert!(
        handle
            .tools_expanded_cell()
            .load(std::sync::atomic::Ordering::SeqCst),
        "an extension `setToolsExpanded(true)` writes the TUI cell"
    );
    assert!(handle.tools_expanded());

    // Extension -> shared cell (false round-trip).
    let result = handle
        .call_tool("expanded_probe", "c2", &json!({ "value": false }))
        .await
        .expect("probe call");
    assert_eq!(tool_text(&result), r#"{"before":true,"after":false}"#);
    assert!(!handle
        .tools_expanded_cell()
        .load(std::sync::atomic::Ordering::SeqCst));

    // TUI -> extension: a write on the shared cell (what Ctrl+O does) is
    // visible to the NEXT `ctx.ui.getToolsExpanded()` call, no JS-local state.
    handle.set_tools_expanded(true);
    let result = handle
        .call_tool("expanded_probe", "c3", &json!({}))
        .await
        .expect("probe call");
    assert_eq!(
        tool_text(&result),
        r#"{"before":true,"after":null}"#,
        "a TUI-side toggle is visible to the extension on the next call"
    );

    handle
        .tools_expanded_cell()
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let result = handle
        .call_tool("expanded_probe", "c4", &json!({}))
        .await
        .expect("probe call");
    assert_eq!(tool_text(&result), r#"{"before":false,"after":null}"#);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Gap 2: ui_prompt_start / ui_prompt_end around every dialog outcome
// ---------------------------------------------------------------------------

const DIALOG_TS: &str = r#"
export default function (pi) {
  globalThis.__promptEvents = [];
  const record = (event) => { globalThis.__promptEvents.push(event); };
  pi.on("ui_prompt_start", record);
  pi.on("ui_prompt_end", record);

  pi.registerTool({
    name: "ask",
    description: "runs one ctx.ui dialog",
    parameters: {
      type: "object",
      properties: {
        kind: { type: "string" },
        timeout: { type: "number" },
      },
    },
    async execute(_id, params, _signal, _onUpdate, ctx) {
      const opts = params.timeout != null ? { timeout: params.timeout } : undefined;
      let out;
      if (params.kind === "confirm") out = await ctx.ui.confirm("Proceed?", "sure?");
      else if (params.kind === "input") out = await ctx.ui.input("Ask", "type here", opts);
      else out = await ctx.ui.select("Pick one", ["a", "b"], opts);
      return { content: [{ type: "text", text: String(out) }] };
    },
  });

  pi.registerTool({
    name: "bad_select",
    description: "a dialog call whose prelude wrapper throws (error outcome)",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      // `options.map` throws: the `finally` in withUIPrompt must still emit
      // exactly one ui_prompt_end.
      await ctx.ui.select("Bad", 5);
      return { content: [{ type: "text", text: "unreachable" }] };
    },
  });
}
"#;

async fn prompt_events(handle: &ExtensionHandle) -> Value {
    handle
        .eval_json("JSON.stringify(globalThis.__promptEvents)")
        .await
        .expect("eval prompt events")
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_ok_emits_start_then_end_exactly_once() {
    let (handle, dir) = started_handle("prompt-ok", true).await;
    seed_and_load(&handle, &dir, "dialog.ts", DIALOG_TS).await;

    let worker = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .call_tool("ask", "d-ok", &json!({ "kind": "confirm" }))
                .await
                .expect("tool call completes")
        }
    });
    let (id, kind, request) = take_dialog(&handle).await;
    assert_eq!(kind, "confirm");
    assert_eq!(request["title"], "Proceed?");
    assert_eq!(request["options"]["message"], "sure?");
    handle.respond_dialog(&id.to_string(), json!(true));

    let result = worker.await.expect("worker joins");
    assert_eq!(tool_text(&result), "true");

    assert_eq!(
        prompt_events(&handle).await,
        json!([
            {
                "type": "ui_prompt_start",
                "reason": "ui_prompt",
                "kind": "confirm",
                "title": "Proceed?"
            },
            {
                "type": "ui_prompt_end",
                "reason": "ui_prompt",
                "kind": "confirm",
                "title": "Proceed?"
            }
        ]),
        "ok outcome: one start then one end, same payload fields"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_cancel_still_emits_end_exactly_once() {
    let (handle, dir) = started_handle("prompt-cancel", true).await;
    seed_and_load(&handle, &dir, "dialog.ts", DIALOG_TS).await;

    let worker = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .call_tool("ask", "d-cancel", &json!({ "kind": "select" }))
                .await
                .expect("tool call completes")
        }
    });
    let (id, kind, _request) = take_dialog(&handle).await;
    assert_eq!(kind, "select");
    // Esc/cancel reply: the waiter resolves null -> undefined.
    handle.respond_dialog(&id.to_string(), Value::Null);

    let result = worker.await.expect("worker joins");
    assert_eq!(tool_text(&result), "undefined");

    assert_eq!(
        prompt_events(&handle).await,
        json!([
            {
                "type": "ui_prompt_start",
                "reason": "ui_prompt",
                "kind": "select",
                "title": "Pick one"
            },
            {
                "type": "ui_prompt_end",
                "reason": "ui_prompt",
                "kind": "select",
                "title": "Pick one"
            }
        ]),
        "cancel outcome: the end event still fires exactly once"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_timeout_still_emits_end_exactly_once() {
    let (handle, dir) = started_handle("prompt-timeout", true).await;
    seed_and_load(&handle, &dir, "dialog.ts", DIALOG_TS).await;

    // No reply: the request-level timeout resolves the waiter null.
    let result = handle
        .call_tool(
            "ask",
            "d-timeout",
            &json!({ "kind": "input", "timeout": 150 }),
        )
        .await
        .expect("tool call completes");
    assert_eq!(tool_text(&result), "undefined");

    assert_eq!(
        prompt_events(&handle).await,
        json!([
            {
                "type": "ui_prompt_start",
                "reason": "ui_prompt",
                "kind": "input",
                "title": "Ask"
            },
            {
                "type": "ui_prompt_end",
                "reason": "ui_prompt",
                "kind": "input",
                "title": "Ask"
            }
        ]),
        "timeout outcome: the end event still fires exactly once"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_error_still_emits_end_exactly_once() {
    let (handle, dir) = started_handle("prompt-error", true).await;
    seed_and_load(&handle, &dir, "dialog.ts", DIALOG_TS).await;

    // The dialog call itself throws inside the withUIPrompt body; the
    // runtime reports a failed tool call and the `finally` still emits end.
    let _error = handle
        .call_tool("bad_select", "d-error", &json!({}))
        .await
        .expect_err("the malformed dialog call fails the tool");

    assert_eq!(
        prompt_events(&handle).await,
        json!([
            {
                "type": "ui_prompt_start",
                "reason": "ui_prompt",
                "kind": "select",
                "title": "Bad"
            },
            {
                "type": "ui_prompt_end",
                "reason": "ui_prompt",
                "kind": "select",
                "title": "Bad"
            }
        ]),
        "error outcome: the end event still fires exactly once"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
