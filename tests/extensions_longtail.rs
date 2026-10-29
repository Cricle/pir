//! Integration tests for the long-tail extension API surface (independent
//! from `extensions_runtime.rs`, which owns the general runtime coverage):
//!
//! 1. `cache_warming_decision` merge semantics (upstream
//!    `runner.ts::emitCacheWarmingDecision` — last `{action}` override wins).
//! 2. `ctx.getSystemPromptOptions()` returning real session values
//!    (upstream `BuildSystemPromptOptions`, minimal subset `{customPrompt?,
//!    cwd}`).
//! 3. `ctx.ui.onTerminalInput` registration semantics and
//!    `ctx.ui.getToolsExpanded`/`setToolsExpanded` state round-trip.
//!
//! Fixtures are generated at runtime into a per-test temp directory (same
//! pattern as `extensions_runtime.rs`: the hygiene gate forbids tracked
//! `.ts` fixtures) or injected with `seed_module`.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::json;

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-ext-longtail-{}-{test_name}",
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

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a custom test prompt.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// A config whose session carries no custom system-prompt override (empty
/// string = default preamble upstream).
fn test_config_default_prompt() -> ExtensionConfig {
    ExtensionConfig {
        system_prompt: String::new(),
        limits: Some(Default::default()),
        ..test_config_at(&std::env::temp_dir())
    }
}

async fn new_runtime_at(cwd: &Path) -> ExtensionRuntime {
    ExtensionRuntime::new(test_config_at(cwd))
        .await
        .expect("runtime construction")
}

// ---------------------------------------------------------------------------
// ctx.getSystemPromptOptions (upstream types.ts:403)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn get_system_prompt_options_returns_real_values() {
    let cwd = write_temp_fixture("spo", &[]);
    let rt = new_runtime_at(&cwd).await;
    rt.seed_module(
        "pir-ext:spo.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (event, ctx) => {
                globalThis.__spo = ctx.getSystemPromptOptions();
            });
        };
        "#,
    )
    .expect("seed");
    let loaded = rt
        .seeded_load("pir-ext:spo.ts")
        .await
        .expect("load seeded extension");
    assert_eq!(loaded, 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let spo: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__spo)")
        .await
        .expect("eval");
    assert_eq!(spo["cwd"], cwd.display().to_string());
    assert_eq!(spo["customPrompt"], "You are a custom test prompt.");
}

#[tokio::test(flavor = "current_thread")]
async fn get_system_prompt_options_omits_custom_prompt_when_default() {
    let rt = ExtensionRuntime::new(test_config_default_prompt())
        .await
        .expect("runtime construction");
    rt.seed_module(
        "pir-ext:spo-default.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (event, ctx) => {
                globalThis.__spo = ctx.getSystemPromptOptions();
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:spo-default.ts")
        .await
        .expect("load seeded extension");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    // No custom prompt override: only `cwd` is reported (upstream marks the
    // other fields optional; an empty override means the default preamble).
    let spo: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__spo)")
        .await
        .expect("eval");
    assert!(spo.get("customPrompt").is_none(), "{spo}");
    assert!(spo["cwd"].is_string(), "{spo}");
}

// ---------------------------------------------------------------------------
// ctx.ui.onTerminalInput / getToolsExpanded / setToolsExpanded (types.ts:163,
// 296-299)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ui_tools_expanded_state_roundtrip() {
    let cwd = write_temp_fixture("tools-expanded", &[]);
    let rt = new_runtime_at(&cwd).await;
    rt.seed_module(
        "pir-ext:expanded.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (event, ctx) => {
                globalThis.__default = ctx.ui.getToolsExpanded();
                ctx.ui.setToolsExpanded(true);
                globalThis.__set = ctx.ui.getToolsExpanded();
                ctx.ui.setToolsExpanded(false);
                globalThis.__reset = ctx.ui.getToolsExpanded();
                // Non-boolean input coerces to false (state stays well-formed).
                ctx.ui.setToolsExpanded("yes");
                globalThis.__coerced = ctx.ui.getToolsExpanded();
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:expanded.ts")
        .await
        .expect("load seeded extension");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let seen: serde_json::Value = rt
        .eval_json(
            "JSON.stringify({
                default: globalThis.__default,
                set: globalThis.__set,
                reset: globalThis.__reset,
                coerced: globalThis.__coerced,
            })",
        )
        .await
        .expect("eval");
    assert_eq!(seen["default"], false);
    assert_eq!(seen["set"], true);
    assert_eq!(seen["reset"], false);
    assert_eq!(seen["coerced"], false);
}

#[tokio::test(flavor = "current_thread")]
async fn ui_on_terminal_input_registration_shim() {
    let cwd = write_temp_fixture("terminal-input", &[]);
    let rt = new_runtime_at(&cwd).await;
    rt.seed_module(
        "pir-ext:terminal.ts",
        r#"
        export default (pi) => {
            pi.on("session_start", (event, ctx) => {
                globalThis.__bad_error = "";
                try {
                    ctx.ui.onTerminalInput("not a function");
                } catch (error) {
                    globalThis.__bad_error = error && error.message
                        ? error.message
                        : String(error);
                }
                // Registration-style: returns the unsubscribe function the
                // upstream API returns (safe to call twice). The TUI feeds
                // chunks through the host's `terminal_input` bridge; live
                // dispatch/consume semantics are covered by
                // `tests/terminal_input_hooks.rs`.
                globalThis.__is_function = typeof ctx.ui.onTerminalInput(() => {}) === "function";
                const unsub = ctx.ui.onTerminalInput(() => {});
                unsub();
                unsub();
                globalThis.__unsub_ok = true;
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:terminal.ts")
        .await
        .expect("load seeded extension");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let seen: serde_json::Value = rt
        .eval_json(
            "JSON.stringify({
                badError: globalThis.__bad_error,
                isFunction: globalThis.__is_function,
                unsubOk: globalThis.__unsub_ok,
            })",
        )
        .await
        .expect("eval");
    assert!(seen["badError"].as_str().unwrap_or("").contains("function"));
    assert_eq!(seen["isFunction"], true);
    assert_eq!(seen["unsubOk"], true);
}

// ---------------------------------------------------------------------------
// cache_warming_decision merge semantics (upstream runner.ts:1121)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn cache_warming_decision_stop_overrides() {
    let cwd = write_temp_fixture("cw-stop", &[]);
    let rt = new_runtime_at(&cwd).await;
    rt.seed_module(
        "pir-ext:cw-stop.ts",
        r#"
        export default (pi) => {
            pi.on("cache_warming_decision", (event, ctx) => ({ action: "stop" }));
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:cw-stop.ts")
        .await
        .expect("load seeded extension");

    let mut payload = json!({ "type": "cache_warming_decision", "action": "warm" });
    let outcome = rt
        .emit("cache_warming_decision", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result["action"], "stop");
    // pi's decision rides the event untouched (handlers observe it).
    assert_eq!(payload["action"], "warm");
}

#[tokio::test(flavor = "current_thread")]
async fn cache_warming_decision_last_override_wins() {
    let cwd = write_temp_fixture("cw-last", &[]);
    let rt = new_runtime_at(&cwd).await;
    // Two handlers in registration order: the later `warm` overrides the
    // earlier `stop` (upstream chains `action` across handlers).
    rt.seed_module(
        "pir-ext:cw-last.ts",
        r#"
        export default (pi) => {
            pi.on("cache_warming_decision", () => ({ action: "stop" }));
            pi.on("cache_warming_decision", () => ({ action: "warm" }));
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:cw-last.ts")
        .await
        .expect("load seeded extension");

    let mut payload = json!({ "type": "cache_warming_decision", "action": "warm" });
    let outcome = rt
        .emit("cache_warming_decision", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result["action"], "warm");
}

#[tokio::test(flavor = "current_thread")]
async fn cache_warming_decision_invalid_action_ignored() {
    let cwd = write_temp_fixture("cw-invalid", &[]);
    let rt = new_runtime_at(&cwd).await;
    rt.seed_module(
        "pir-ext:cw-invalid.ts",
        r#"
        export default (pi) => {
            pi.on("cache_warming_decision", (event, ctx) => {
                globalThis.__seen_action = event.action;
                globalThis.__seen_type = event.type;
                globalThis.__seen_thinking = ctx.thinkingLevel !== undefined;
                // Only "warm"/"stop" are honored; anything else leaves pi's
                // decision untouched (no override).
                return { action: "PAUSE_EVERYTHING" };
            });
        };
        "#,
    )
    .expect("seed");
    rt.seeded_load("pir-ext:cw-invalid.ts")
        .await
        .expect("load seeded extension");

    let mut payload = json!({ "type": "cache_warming_decision", "action": "warm" });
    let outcome = rt
        .emit("cache_warming_decision", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    // No valid override: no decision flows back (fail-open to pi's warm).
    assert!(outcome.result.is_null(), "{:?}", outcome.result);

    let seen: serde_json::Value = rt
        .eval_json(
            "JSON.stringify({ action: globalThis.__seen_action, type: globalThis.__seen_type })",
        )
        .await
        .expect("eval");
    assert_eq!(seen["action"], "warm");
    assert_eq!(seen["type"], "cache_warming_decision");
}

#[tokio::test(flavor = "current_thread")]
async fn cache_warming_decision_without_handlers_yields_no_result() {
    let rt = ExtensionRuntime::new(test_config_default_prompt())
        .await
        .expect("runtime construction");
    let mut payload = json!({ "type": "cache_warming_decision", "action": "warm" });
    let outcome = rt
        .emit("cache_warming_decision", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.result.is_null(), "{:?}", outcome.result);
}
