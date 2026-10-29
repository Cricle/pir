//! Regression test: the TUI render path runs synchronously on the tokio
//! runtime thread (`main.rs` drives `run()` with `block_on`; the terminal
//! render helpers call `RendererInvoker::invoke` inline). The old bridge used
//! `mpsc::Sender::blocking_send`, which panics ("Cannot block the current
//! thread from within a runtime") the moment any extension registers a
//! renderer. `ExtensionHandle::invoke_renderer_sync` must now succeed from
//! inside a runtime context and degrade to `Err` — never panic — when a
//! renderer overruns its timeout.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::tui::renderers::{
    ExtensionRendererInvoker, RendererInvoker, RendererKind, RendererRequest,
};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-renderer-runtime-fix-{label}-{}-{}",
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
        model: Some("runtime-fix-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
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

/// Invoke on a tokio runtime thread — the exact TUI call shape. Under the
/// old `blocking_send` bridge this panicked instead of returning.
#[test]
fn renderer_invoke_inside_tokio_runtime_does_not_panic() {
    let temp = TempDir::new("runtime-invoke");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "wrap.ts",
        r#"
        export default function (pi) {
          pi.registerMarkdownTransformer("wrap", (markdown) => "R(" + markdown + ")");
        }
        "#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let outcome = runtime.block_on(async {
        invoker.invoke(&RendererRequest {
            kind: RendererKind::Markdown,
            key: String::new(),
            payload: json!({ "markdown": "hi" }),
            options: json!({ "messageType": "user", "width": 60 }),
        })
    });
    let outcome = outcome.expect("invoke inside the runtime must not panic");
    assert_eq!(
        outcome.text.as_deref(),
        Some("R(hi)"),
        "registered transformer must run when invoked from a runtime thread"
    );
}

/// A renderer that overruns its timeout must return `Err` (the call sites
/// degrade to the default rendering) and leave the actor usable — the old
/// bridge panicked before any of that could happen.
#[test]
fn runaway_renderer_from_runtime_degrades_to_err() {
    let temp = TempDir::new("runtime-timeout");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "spin.ts",
        r#"
        export default function (pi) {
          // Bounded busy loop; the actor's QuickJS interrupt deadline must
          // abort it far before the 10s cap.
          pi.registerMarkdownTransformer("spin", () => {
            const stop = Date.now() + 10000;
            while (Date.now() < stop) {}
            return "late";
          });
        }
        "#,
    );
    let request = RendererRequest {
        kind: RendererKind::Markdown,
        key: String::new(),
        payload: json!({ "markdown": "hi" }),
        options: json!({ "messageType": "user", "width": 60 }),
    };
    let request_json = serde_json::to_value(&request).expect("serialize request");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let result = runtime
        .block_on(async { handle.invoke_renderer_sync(&request_json, Duration::from_millis(80)) });
    let error = result.expect_err("a runaway renderer must degrade to Err, not panic");
    // The reply carries either the actor's interrupt abort ("...interrupted /
    // timed out") or, if the actor were wedged past the slack window, the
    // local recv_timeout error; both are Err and both render as a notice.
    let message = error.to_string();
    assert!(
        !message.is_empty(),
        "degradation error must carry a message, got: {message}"
    );
    assert!(
        handle.is_ready(),
        "the actor must survive the aborted renderer call"
    );
}
