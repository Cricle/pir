//! Builtin-only extension plans must not construct the QuickJS extension
//! runtime.
//!
//! Measured before the fix: `pir print` with the default (builtin-only)
//! extension plan peaked at 32.4 MB while `--no-extensions` peaked at
//! 21.8 MB. Disabling individual builtins changed nothing, because the cost
//! was the extension host itself: `ExtensionHandle::start` spawned the actor,
//! and the actor built `ExtensionRuntime` (QuickJS + the 223 KB JS prelude)
//! even though the plan contained only `builtin:<name>` pseudo-paths.
//!
//! `load_extension_runtime` now partitions the plan first and builds a
//! plan-only handle (shared `RuntimeState` + path plan, no actor/QuickJS) when
//! the on-disk load list is empty. This test pins that seam:
//!
//! 1. a builtin-only plan leaves [`pir::extensions::runtime::js_runtime_build_count`]
//!    unchanged and `ExtensionHandle::has_js_runtime()` false, while Rust
//!    builtins still register and event dispatch behaves like the old empty
//!    JS runtime;
//! 2. a plan carrying a real on-disk JS extension still constructs the
//!    runtime, loads the extension, runs its hook and hands it `ctx`
//!    (the broader JS-extension suites — `tests/suites/extensions_events.rs`
//!    `ctx_facade_reports_session_model_and_thinking` /
//!    `loop_events_reach_extensions`, `tests/extensions_runtime.rs`,
//!    `tests/suites/js_extension_api.rs` — cover the full surface).

use std::path::{Path, PathBuf};

use pir::extensions::ExtensionConfig;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-builtin-only-{label}-{}-{}",
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

fn config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_only_plan_skips_quickjs_but_js_extensions_still_run() {
    let temp = TempDir::new("plan");
    let names = pir::plugins::builtin_names();
    if names.is_empty() {
        // A minimal build compiles no builtin factories; nothing to exercise.
        return;
    }
    let paths: Vec<PathBuf> = names
        .iter()
        .map(|name| PathBuf::from(format!("builtin:{name}")))
        .collect();

    // --- 1. Builtin-only plan: no QuickJS runtime -------------------------
    let baseline = pir::extensions::runtime::js_runtime_build_count();
    let handle = pir::agent_session::load_extension_runtime(&paths, config(&temp.0))
        .await
        .expect("a builtin-only plan still yields a runtime handle");
    assert_eq!(
        pir::extensions::runtime::js_runtime_build_count(),
        baseline,
        "a builtin-only extension plan must not construct the QuickJS runtime"
    );
    assert!(
        !handle.has_js_runtime(),
        "the builtin-only handle must be plan-only (no actor thread)"
    );
    assert!(
        handle.is_ready(),
        "the plan-only handle is ready for its surface"
    );
    assert_eq!(
        handle.path_plan(),
        paths,
        "the plan is preserved for the plugin host"
    );
    assert!(
        handle.tool_defs().is_empty() && handle.command_defs().is_empty(),
        "no JS extension loaded, so no JS registrations exist"
    );

    // Rust builtins still instantiate/register through the unified plugin host.
    let host = pir::plugins::PluginHost::build(Some(handle.clone()), Vec::new());
    let plugin_paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    for name in &names {
        assert!(
            plugin_paths.contains(&format!("builtin:{name}")),
            "builtin `{name}` is missing from the plugin host: {plugin_paths:?}"
        );
    }

    // Dispatch behaves exactly like the old empty JS runtime: no handlers, no
    // errors and an untouched payload.
    let mut payload = serde_json::json!({ "type": "session_start", "reason": "test" });
    let outcome = handle
        .emit("session_start", &mut payload)
        .await
        .expect("plan-only emit");
    assert!(outcome.result.is_null(), "no handler can produce a result");
    assert!(outcome.errors.is_empty(), "no handler can produce an error");
    assert_eq!(
        payload,
        serde_json::json!({ "type": "session_start", "reason": "test" }),
        "no handler can mutate the payload"
    );

    // --- 2. A real JS extension still gets the full runtime ---------------
    #[cfg(feature = "extensions")]
    {
        let entry = temp.0.join("hook.js");
        std::fs::write(
            &entry,
            r#"
export default function (pi) {
    pi.on("session_start", async (event, ctx) => {
        globalThis.__sawCtx = { cwd: ctx.cwd, mode: ctx.mode };
    });
}
"#,
        )
        .expect("write extension fixture");

        let before = pir::extensions::runtime::js_runtime_build_count();
        let js_paths = vec![PathBuf::from(format!("builtin:{}", names[0])), entry];
        let js_handle = pir::agent_session::load_extension_runtime(&js_paths, config(&temp.0))
            .await
            .expect("a JS extension plan starts the runtime");
        assert!(
            js_handle.has_js_runtime(),
            "an on-disk JS extension must keep the QuickJS runtime"
        );
        assert!(
            pir::extensions::runtime::js_runtime_build_count() > before,
            "a JS extension must construct the QuickJS runtime"
        );

        let mut payload = serde_json::json!({ "type": "session_start", "reason": "test" });
        js_handle
            .emit("session_start", &mut payload)
            .await
            .expect("session_start reaches the JS handler");
        let seen = js_handle
            .eval_json("JSON.stringify(globalThis.__sawCtx ?? null)")
            .await
            .expect("probe the hook's ctx snapshot");
        assert_eq!(
            seen["cwd"],
            serde_json::json!(temp.0.display().to_string()),
            "the hook must observe ctx.cwd"
        );
        assert_eq!(
            seen["mode"],
            serde_json::json!("print"),
            "the hook must observe the normalized ctx.mode (`headless` -> `print`)"
        );
    }
}
