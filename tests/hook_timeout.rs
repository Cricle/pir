//! Long-deadline behaviour of the reserved-prefix hook channels
//! (`__pir_stream:` / `__pir_route:`) versus the short per-call timeout kept
//! for ordinary tool `prepareLoadout` hooks (review P1-1 fix).
//!
//! `ExtensionHandle::set_call_timeout` shrinks the configured per-call
//! deadline to well under the JS hooks' sleep. The reserved hook calls must
//! ignore it and run under the long hook deadline
//! ([`pir::extensions::runtime::HOOK_CALL_TIMEOUT`], 30 minutes) — a real
//! model backend can keep one `streamSimple` round trip running for many
//! minutes — while a genuine tool's `prepareLoadout` hook must still be
//! aborted by the short deadline.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::time::{Duration, Instant};

use serde_json::json;

use pir::extensions::{ExtensionConfig, ExtensionHandle};

/// Short per-call deadline the tests install via `set_call_timeout`.
const SHORT_TIMEOUT: Duration = Duration::from_millis(150);
/// The JS hooks sleep longer than [`SHORT_TIMEOUT`]; completing at all
/// therefore proves the call was not bound by the short per-call deadline.
const HOOK_SLEEP_MS: u64 = 600;

fn js_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Per-test temp directory removed at the end of each test body.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-hook-timeout-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One extension registering a sleeping `streamSimple` callback, a sleeping
/// virtual-model `route` callback, and a tool whose `prepareLoadout` sleeps.
const HOOK_EXTENSION_TS: &str = r#"
export default function (pi) {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  pi.registerProvider("hookprov", {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: "hook-model", name: "Hook model", contextWindow: 1000, maxTokens: 100, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    streamSimple: async () => {
      await sleep(600);
      return [];
    },
  });
  pi.registerVirtualModel({
    id: "hookrouter",
    physical: { provider: "fake", id: "fake-model" },
    route: async () => {
      await sleep(600);
      return { model: { provider: "fake", id: "fake-model" } };
    },
  });
  pi.registerTool({
    name: "slowLoadoutTool",
    description: "tool whose prepareLoadout hook sleeps",
    parameters: { type: "object", properties: {} },
    execute: async () => "ok",
    prepareLoadout: async () => {
      await sleep(600);
      return undefined;
    },
  });
}
"#;

async fn load_hook_extension(label: &str) -> (std::path::PathBuf, ExtensionHandle) {
    let dir = temp_dir(label);
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    let path = dir.join("ext.ts");
    std::fs::write(&path, HOOK_EXTENSION_TS).unwrap();
    let result = handle.load(std::slice::from_ref(&path)).await.unwrap();
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
    (dir, handle)
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_stream_hook_runs_under_the_long_hook_deadline() {
    let (dir, handle) = load_hook_extension("stream").await;
    handle.set_call_timeout(SHORT_TIMEOUT);

    let started = Instant::now();
    let response = handle
        .call_prepare_loadout("__pir_stream:hookprov", &json!({}))
        .await;
    let elapsed = started.elapsed();

    let response =
        response.expect("stream hook must not time out under the short per-call deadline");
    let events = response
        .expect("stream hook must produce a payload")
        .get("events")
        .and_then(serde_json::Value::as_array)
        .expect("stream hook response carries an events array")
        .clone();
    assert!(events.is_empty(), "streamSimple returned no events");
    assert!(
        elapsed >= Duration::from_millis(HOOK_SLEEP_MS),
        "stream hook ran {elapsed:?}; it must complete past the short per-call deadline"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_route_hook_runs_under_the_long_hook_deadline() {
    let (dir, handle) = load_hook_extension("route").await;
    handle.set_call_timeout(SHORT_TIMEOUT);

    let started = Instant::now();
    let response = handle
        .call_prepare_loadout("__pir_route:fake/hookrouter", &json!({}))
        .await;
    let elapsed = started.elapsed();

    let response =
        response.expect("route hook must not time out under the short per-call deadline");
    let decision = response.expect("route hook must produce a decision");
    assert_eq!(
        decision
            .get("physical")
            .and_then(|physical| physical.get("id"))
            .and_then(serde_json::Value::as_str),
        Some("fake-model"),
        "route callback decision came through the reserved channel"
    );
    assert!(
        elapsed >= Duration::from_millis(HOOK_SLEEP_MS),
        "route hook ran {elapsed:?}; it must complete past the short per-call deadline"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_tool_prepare_loadout_keeps_the_short_timeout() {
    let (dir, handle) = load_hook_extension("tool").await;
    handle.set_call_timeout(SHORT_TIMEOUT);

    let started = Instant::now();
    let result = handle
        .call_prepare_loadout("slowLoadoutTool", &json!({}))
        .await;
    let elapsed = started.elapsed();

    let error = result.expect_err("sleeping tool prepareLoadout must time out");
    assert!(error.contains("timed out"), "timeout error: {error}");
    assert!(
        elapsed < Duration::from_millis(HOOK_SLEEP_MS),
        "tool hook aborted at the short deadline, not after the JS sleep ({elapsed:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
