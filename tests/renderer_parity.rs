//! Renderer-extension parity tests (upstream
//! `packages/coding-agent/src/core/extensions/types.ts:661-666` +
//! `runner.ts:775-805` + `modes/interactive/components/markdown-transform.ts`).
//!
//! Drives the real QuickJS extension runtime through the same
//! `RendererInvoker` seam the TUI consumes:
//!
//! - message renderers fire for custom **message** entries
//!   (`sendMessage` → `CustomMessageEntry` → `messageRenderers`, matched by
//!   customType) while custom **entries** stay on the entry bucket
//!   (`appendEntry` → `CustomEntry` → `entryRenderers`) — the split
//!   `render_custom_entry` applies via `WorkerEvent::CustomEntry.is_message`;
//! - tool renderer resolvers chain per tool name across extensions in load
//!   order, each able to delegate through `next()`
//!   (`runner.ts resolveToolRenderers`);
//! - markdown transformers compose in extension load order, each output
//!   feeding the next; a throwing transformer keeps the current markdown and
//!   the chain continues (`applyMarkdownTransformers`).
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::PathBuf;
use std::sync::Arc;

use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::tui::renderers::{
    renderer_registered, ExtensionRendererInvoker, RendererInvoker, RendererKind, RendererRequest,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures (same real-QuickJS pattern as tests/suites/extensions_events.rs)
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-renderer-parity-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start_handle(dir: &std::path::Path) -> ExtensionHandle {
    ExtensionHandle::start(ExtensionConfig {
        cwd: dir.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui: true,
        model: Some("parity-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
        allow_process_exit: false,
        agent_dir: None,
    })
    .expect("runtime start")
}

fn load_extension(handle: &ExtensionHandle, dir: &std::path::Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let path = dir.join(name);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let result = runtime
        .block_on(handle.load(std::slice::from_ref(&path)))
        .expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

fn invoke(
    invoker: &dyn RendererInvoker,
    kind: RendererKind,
    key: &str,
    payload: Value,
) -> anyhow::Result<pir::tui::renderers::RendererOutcome> {
    invoker.invoke(&RendererRequest {
        kind,
        key: key.to_string(),
        payload,
        options: json!({ "expanded": false, "width": 60, "messageType": "user" }),
    })
}

// ---------------------------------------------------------------------------
// Message vs entry split (fix 1: custom message entries reach the
// messageRenderers bucket instead of the entry bucket)
// ---------------------------------------------------------------------------

#[test]
fn message_renderer_fires_for_custom_message_entries() {
    let temp = TempDir::new("message-split");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "renderers.ts",
        r#"
        export default function (pi) {
          pi.registerMessageRenderer("plan", (message, options, theme) => {
            return ["PLAN: " + message.title, "steps: " + (message.steps ?? 0)];
          });
          pi.registerEntryRenderer("widget", (entry, options, theme) => {
            return ["WIDGET " + entry.data.name];
          });
        }
        "#,
    );
    let handle = Arc::new(handle);
    let invoker = ExtensionRendererInvoker::new(handle.clone());

    // Registry resolution: the message customType lands in the message
    // bucket, the entry customType in the entry bucket.
    let defs = handle.renderer_defs();
    assert!(
        renderer_registered(&defs, RendererKind::Message, "plan"),
        "custom message entry must consult the messageRenderers bucket"
    );
    assert!(renderer_registered(&defs, RendererKind::Entry, "widget"));

    // `sendMessage` payloads route through RendererKind::Message (upstream
    // interactive-mode.ts:3891 getMessageRenderer(message.customType)).
    let outcome = invoke(
        &invoker,
        RendererKind::Message,
        "plan",
        json!({ "customType": "plan", "title": "Refactor", "steps": 3 }),
    )
    .expect("message renderer invoke");
    assert_eq!(
        outcome.lines,
        Some(vec!["PLAN: Refactor".to_string(), "steps: 3".to_string()]),
        "registered message renderer must render custom message entries"
    );

    // `appendEntry` payloads keep the Entry bucket (display-only).
    let outcome = invoke(
        &invoker,
        RendererKind::Entry,
        "widget",
        json!({ "customType": "widget", "data": { "name": "status" } }),
    )
    .expect("entry renderer invoke");
    assert_eq!(outcome.lines, Some(vec!["WIDGET status".to_string()]));

    // The buckets do not cross: a message renderer must not answer an entry
    // request and vice versa (upstream keeps the two registries separate).
    let outcome =
        invoke(&invoker, RendererKind::Entry, "plan", json!({})).expect("cross-kind entry invoke");
    assert!(
        outcome.is_unhandled(),
        "entry lookup must miss message bucket"
    );
    let outcome = invoke(&invoker, RendererKind::Message, "widget", json!({}))
        .expect("cross-kind message invoke");
    assert!(
        outcome.is_unhandled(),
        "message lookup must miss entry bucket"
    );
}

// ---------------------------------------------------------------------------
// Tool renderer resolver chain (fix 2: multiple resolvers per tool name,
// next() delegation in extension load order)
// ---------------------------------------------------------------------------

#[test]
fn tool_renderer_resolvers_chain_in_load_order_with_next() {
    let temp = TempDir::new("tool-chain");
    let handle = start_handle(&temp.0);
    // Extension 1 registers TWO resolvers for the same tool name (the old
    // first-wins guard would have dropped the second), then extension 2 adds
    // a third. Load order: ext1 resolver a, ext1 resolver b, ext2 resolver c.
    load_extension(
        &handle,
        &temp.0,
        "one.ts",
        r#"
        export default function (pi) {
          pi.registerToolRenderer("bash", async (tool, next) => {
            const down = await next();
            return down ? ["a[" + down.join(",") + "]"] : ["a[]"];
          });
          pi.registerToolRenderer("bash", async (tool, next) => {
            const down = await next();
            return down ? down : ["b"];
          });
        }
        "#,
    );
    load_extension(
        &handle,
        &temp.0,
        "two.ts",
        r#"
        export default function (pi) {
          pi.registerToolRenderer("bash", async (tool, next) => {
            const down = await next();
            return down ? down : ["c"];
          });
        }
        "#,
    );
    let defs = handle.renderer_defs();
    assert!(
        renderer_registered(&defs, RendererKind::Tool, "bash"),
        "exported tool renderer defs must list every chained resolver"
    );
    let chain = defs["toolRenderers"]
        .as_array()
        .expect("flat toolRenderers array")
        .iter()
        .filter(|entry| entry["toolName"] == json!("bash"))
        .count();
    assert_eq!(
        chain, 3,
        "all three resolvers must be registered, got {chain}"
    );

    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));
    let outcome = invoke(
        &invoker,
        RendererKind::Tool,
        "bash",
        json!({ "toolName": "bash", "args": { "command": "ls" } }),
    )
    .expect("tool renderer chain invoke");
    // a wraps b's fallthrough... b delegates to c (last resolver, next() ends
    // the chain), c produces ["c"], b passes it through, a wraps it.
    assert_eq!(
        outcome.lines,
        Some(vec!["a[c]".to_string()]),
        "resolver chain must run in load order with next() delegation"
    );

    // A resolver that ignores next() short-circuits the chain (upstream
    // semantics: returning renderers without calling next() wins).
    let temp2 = TempDir::new("tool-chain-short");
    let handle2 = start_handle(&temp2.0);
    load_extension(
        &handle2,
        &temp2.0,
        "short.ts",
        r#"
        export default function (pi) {
          pi.registerToolRenderer("bash", (tool, next) => ["direct"]);
          pi.registerToolRenderer("bash", async (tool, next) => ["never"]);
        }
        "#,
    );
    let invoker2 = ExtensionRendererInvoker::new(Arc::new(handle2));
    let outcome = invoke(
        &invoker2,
        RendererKind::Tool,
        "bash",
        json!({ "toolName": "bash", "args": {} }),
    )
    .expect("short-circuit invoke");
    assert_eq!(outcome.lines, Some(vec!["direct".to_string()]));
}

// ---------------------------------------------------------------------------
// Markdown transformer composition (fix 3: all transformers chained in
// registration order instead of first-registered-wins)
// ---------------------------------------------------------------------------

#[test]
fn markdown_transformers_compose_in_registration_order() {
    let temp = TempDir::new("markdown-chain");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "one.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer("wrap", (markdown, context) => "W(" + markdown + ")");
          pi.registerMarkdownTransformer("shout", (markdown, context) => markdown.toUpperCase());
        }
        "#,
    );
    load_extension(
        &handle,
        &temp.0,
        "two.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer("tail", (markdown, context) => markdown + "!");
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));
    let outcome = invoke(
        &invoker,
        RendererKind::Markdown,
        "",
        json!({ "markdown": "hi" }),
    )
    .expect("markdown chain invoke");
    // ext1 "wrap" runs first, then ext1 "shout", then ext2 "tail".
    assert_eq!(
        outcome.text.as_deref(),
        Some("W(HI)!"),
        "transformers must chain in registration order, each output feeding the next"
    );
}

#[test]
fn markdown_transformer_failure_keeps_current_markdown_and_continues() {
    let temp = TempDir::new("markdown-chain-error");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "one.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer("boom", () => {
            throw new Error("boom");
          });
        }
        "#,
    );
    load_extension(
        &handle,
        &temp.0,
        "two.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer("safe", (markdown) => markdown + "-safe");
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));
    let outcome = invoke(
        &invoker,
        RendererKind::Markdown,
        "",
        json!({ "markdown": "hi" }),
    )
    .expect("markdown chain invoke");
    // The throwing transformer keeps "hi" (upstream catches and continues),
    // the later transformer still runs.
    assert_eq!(
        outcome.text.as_deref(),
        Some("hi-safe"),
        "a throwing transformer must not abort the chain"
    );
}

// ---------------------------------------------------------------------------
// Host keybinding hints (#11630)
//
// Upstream passes the host's resolved `keyText`/`keyHint` into
// `ToolRenderContext`, `MessageRenderOptions` and `EntryRenderOptions`
// (fixes #4748: an extension's own package copy cannot see the host's
// settings overrides). The TUI resolves the map through
// `Keybindings::keybinding_hints_json` and pushes it with
// `ExtensionHandle::update_keybinding_hints`; the renderer sees the injected
// functions, never the shim's static table.
// ---------------------------------------------------------------------------

#[test]
fn renderers_receive_host_resolved_keybinding_hints() {
    use pir::tui::ansi::strip_ansi;
    use pir::tui::keybindings::Keybindings;

    let temp = TempDir::new("keybinding-hints");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "hints.ts",
        r#"
        import { Type } from "typebox";
        export default function (pi) {
          pi.registerTool({
            name: "hint-tool",
            description: "hint tool",
            parameters: Type.Object({}),
            async execute() {
              return { content: [{ type: "text", text: "ok" }] };
            },
            renderCall(_args, _theme, context) {
              return [`call:${context.keyText("app.tools.expand")}|${context.keyHint("app.tools.expand", "to expand")}`];
            },
          });
          pi.registerMessageRenderer("hint-msg", (_message, options) => {
            return [`msg:${options.keyText("app.tools.expand")}|${options.keyHint("app.tools.expand", "to expand")}`];
          });
          pi.registerEntryRenderer("hint-entry", (_entry, options) => {
            return [`entry:${options.keyText("app.tools.expand")}|${options.keyHint("app.tools.expand", "to expand")}`];
          });
        }
        "#,
    );
    let handle = Arc::new(handle);
    let invoker = ExtensionRendererInvoker::new(Arc::clone(&handle));

    let plain = |lines: Option<Vec<String>>| -> Vec<String> {
        lines
            .expect("renderer lines")
            .iter()
            .map(|line| strip_ansi(line))
            .collect()
    };

    // No host push yet (the headless/RPC shape): the injected functions exist,
    // but every id answers the empty text — same as upstream RPC/print, where
    // app keybindings are never installed.
    let tool_call = handle
        .call_tool_renderer_sync(
            "hint-tool",
            "call",
            &json!({ "args": {}, "toolCallId": "h1", "cwd": "/w" }),
            std::time::Duration::from_secs(2),
        )
        .expect("tool renderer call");
    assert_eq!(plain(tool_call.lines), vec!["call:| to expand".to_string()]);

    // The TUI seam: resolve a user override, then push the map into the
    // extension runtime (startup and `/reload` do exactly this).
    let keybindings = Keybindings::from_settings(Some(&json!({
        "app.tools.expand": "ctrl+e"
    })))
    .expect("keybindings settings");
    let hints = keybindings.keybinding_hints_json();
    assert_eq!(hints["app.tools.expand"], json!("ctrl+e"));
    // Both namespaces are covered: `tui.*` ids resolve through the TUI
    // registry, `app.*` through the app/hint tables.
    assert_eq!(hints["tui.select.confirm"], json!("enter"));
    assert_eq!(hints["app.interrupt"], json!("esc"));
    handle.update_keybinding_hints(hints);

    // Tool renderCall: the overridden chord reaches ToolRenderContext.
    let tool_call = handle
        .call_tool_renderer_sync(
            "hint-tool",
            "call",
            &json!({ "args": {}, "toolCallId": "h2", "cwd": "/w" }),
            std::time::Duration::from_secs(2),
        )
        .expect("tool renderer call");
    assert_eq!(
        plain(tool_call.lines),
        vec!["call:ctrl+e|ctrl+e to expand".to_string()],
        "tool renderers must format hints from the host's configured binding"
    );

    // Message renderer: the overridden chord reaches MessageRenderOptions.
    let message = invoke(
        &invoker,
        RendererKind::Message,
        "hint-msg",
        json!({ "customType": "hint-msg" }),
    )
    .expect("message renderer invoke");
    assert_eq!(
        plain(message.lines),
        vec!["msg:ctrl+e|ctrl+e to expand".to_string()],
        "message renderers must receive keyText/keyHint in their options"
    );

    // Entry renderer: the overridden chord reaches EntryRenderOptions.
    let entry = invoke(
        &invoker,
        RendererKind::Entry,
        "hint-entry",
        json!({ "customType": "hint-entry" }),
    )
    .expect("entry renderer invoke");
    assert_eq!(
        plain(entry.lines),
        vec!["entry:ctrl+e|ctrl+e to expand".to_string()],
        "entry renderers must receive keyText/keyHint in their options"
    );
}

// ---------------------------------------------------------------------------
// Markdown transformer surfaces (upstream v1.5.2 #11756)
//
// `registerMarkdownTransformer(fn, { messageTypes })` selects the surfaces the
// transformer runs on; an unselected surface leaves the markdown unchanged.
// ---------------------------------------------------------------------------

fn invoke_on_surface(invoker: &dyn RendererInvoker, markdown: &str, message_type: &str) -> String {
    let outcome = invoker
        .invoke(&RendererRequest {
            kind: RendererKind::Markdown,
            key: String::new(),
            payload: json!({ "markdown": markdown }),
            options: json!({ "messageType": message_type, "isStreaming": false, "width": 60 }),
        })
        .expect("markdown invoke");
    outcome.text.unwrap_or_else(|| markdown.to_owned())
}

#[test]
fn markdown_transformer_runs_only_on_its_registered_surfaces() {
    let temp = TempDir::new("markdown-surfaces");
    let handle = start_handle(&temp.0);
    // Upstream bare-transformer form with explicit surfaces.
    load_extension(
        &handle,
        &temp.0,
        "summary.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer(
            (markdown) => "S:" + markdown,
            { messageTypes: ["summary"] },
          );
        }
        "#,
    );
    // No options: the transcript trio only.
    load_extension(
        &handle,
        &temp.0,
        "all.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer((markdown) => markdown + "-default");
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));

    // Each transformer runs only on its own surfaces; the two surface sets do
    // not overlap here, so the output names exactly which one ran.
    assert_eq!(
        invoke_on_surface(&invoker, "hi", "summary"),
        "S:hi",
        "the summary transformer runs on summary; the default one does not"
    );
    assert_eq!(
        invoke_on_surface(&invoker, "hi", "user"),
        "hi-default",
        "the default transformer runs on user markdown"
    );
    assert_eq!(
        invoke_on_surface(&invoker, "hi", "assistant-thinking"),
        "hi-default",
        "the transcript trio is the default surface set"
    );
    assert_eq!(
        invoke_on_surface(&invoker, "hi", "changelog"),
        "hi",
        "a surface nobody opted into stays untouched"
    );
}

#[test]
fn markdown_transformer_coerces_explicit_surfaces_to_a_set() {
    let temp = TempDir::new("markdown-surfaces-two");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "two.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer(
            (markdown) => markdown.toUpperCase(),
            { messageTypes: ["custom", "changelog"] },
          );
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));
    assert_eq!(invoke_on_surface(&invoker, "x", "custom"), "X");
    assert_eq!(invoke_on_surface(&invoker, "x", "changelog"), "X");
    assert_eq!(invoke_on_surface(&invoker, "x", "user"), "x");
}
