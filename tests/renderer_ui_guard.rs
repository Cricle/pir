//! L7 regression: an async `ctx.ui.*` request from a renderer callback must
//! fail fast instead of self-starving the TUI for the renderer timeout.
//!
//! Renderer invocations block the TUI main thread
//! (`ExtensionHandle::invoke_renderer_sync`); an interactive dialog requested
//! by the callback parks a `Notification::DialogRequest` whose only drainer is
//! that blocked thread, so the request could never be answered: the old
//! behavior waited out the full renderer deadline and left an orphaned dialog
//! queued for a later frame. The bridge now suppresses the runtime's async UI
//! for the callback's duration, so the request resolves immediately with the
//! headless-host `undefined` and synchronous UI keeps working.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::tui::renderers::{
    ExtensionRendererInvoker, RendererInvoker, RendererKind, RendererRequest,
};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-renderer-ui-guard-{label}-{}-{}",
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
        model: Some("renderer-ui-guard-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start")
}

/// Load `source` and capture a real interactive `ctx` for the renderer
/// closure (renderer callbacks receive no ctx of their own; the fixture's
/// `capture-ui` command stores one on `globalThis`).
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
    runtime
        .block_on(handle.dispatch_command("capture-ui", ""))
        .expect("capture command");
}

fn markdown_request() -> RendererRequest {
    RendererRequest {
        kind: RendererKind::Markdown,
        key: String::new(),
        payload: json!({ "markdown": "hi" }),
        options: json!({ "messageType": "user", "width": 60 }),
    }
}

#[test]
fn async_ui_from_a_renderer_callback_resolves_without_waiting_for_the_deadline() {
    let temp = TempDir::new("async-ui");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "ui.ts",
        r#"
        export default function (pi) {
          pi.registerCommand("capture-ui", {
            handler: async (args, ctx) => { globalThis.__pir_renderer_ctx = ctx; },
          });
          pi.registerMarkdownTransformer("ui", async (markdown) => {
            const ctx = globalThis.__pir_renderer_ctx;
            if (!ctx) return "NO_CTX";
            const answer = await ctx.ui.input("title", "placeholder");
            return answer === undefined ? "UI_UNAVAILABLE" : "UI_ANSWER:" + String(answer);
          });
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle.clone()));
    let request = markdown_request();
    let started = Instant::now();
    let outcome = invoker
        .invoke(&request)
        .expect("renderer with an unavailable async UI must still complete");
    let elapsed = started.elapsed();

    // Without the guard the dialog parks until the renderer deadline aborts
    // the callback (the invoker waits `timeout + 1s` slack). With the guard
    // the dialog host fn resolves null immediately and the callback returns
    // undefined.
    assert!(
        elapsed < Duration::from_millis(1_500),
        "async ctx.ui must fail fast, took {elapsed:?}"
    );
    assert_eq!(outcome.text.as_deref(), Some("UI_UNAVAILABLE"));
    let dialogs = handle.take_notifications(|notification| {
        matches!(notification, Notification::DialogRequest { .. })
    });
    assert!(
        dialogs.is_empty(),
        "no dialog request may be parked for a renderer callback: {dialogs:?}"
    );
    assert!(
        !handle.renderer_callback_in_flight(),
        "the guard must be released when the callback returns"
    );
    assert!(
        handle.config().has_ui,
        "the runtime's has_ui must be restored after the callback"
    );
}

#[test]
fn synchronous_ui_from_a_renderer_callback_keeps_working() {
    let temp = TempDir::new("sync-ui");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "notify.ts",
        r#"
        export default function (pi) {
          pi.registerCommand("capture-ui", {
            handler: async (args, ctx) => { globalThis.__pir_renderer_ctx = ctx; },
          });
          pi.registerMarkdownTransformer("notify", async () => {
            const ctx = globalThis.__pir_renderer_ctx;
            ctx.ui.notify("renderer notice", "info");
            ctx.ui.setStatus("renderer-status", "busy");
            return "NOTIFIED";
          });
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle.clone()));
    let outcome = invoker
        .invoke(&markdown_request())
        .expect("synchronous UI renderer must complete");
    assert_eq!(outcome.text.as_deref(), Some("NOTIFIED"));

    let notifications = handle.take_notifications(|_| true);
    assert!(
        notifications.iter().any(|notification| matches!(
            notification,
            Notification::Notify { message, level }
                if message == "renderer notice" && level == "info"
        )),
        "ctx.ui.notify must keep working inside a renderer: {notifications:?}"
    );
    assert!(
        notifications.iter().any(|notification| matches!(
            notification,
            Notification::Status { key, text }
                if key == "renderer-status" && text.as_deref() == Some("busy")
        )),
        "ctx.ui.setStatus must keep working inside a renderer: {notifications:?}"
    );
}
