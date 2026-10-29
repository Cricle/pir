//! Locks the host-backed `ctx.sessionManager.buildSessionContext()` against
//! its real consumption surface: pi-subagents' `tool-activation.js` calls
//! `ctx.sessionManager.buildSessionContext().messages` (unprotected — a
//! missing method throws inside the `session_start` handler and silently
//! degrades dynamic tool activation to the eager default).
//!
//! The consumed contract (tool-activation.js `applyRecordedSelection`):
//!
//! - `buildSessionContext()` returns `{ messages }` — an array;
//! - a system message "declares native tool selection" when
//!   `Object.hasOwn(message, "toolsAdded" | "toolsRemoved")` (so the keys
//!   must be PRESENT exactly when the host recorded a delta);
//! - `message.toolsAdded ?? []` / `toolsRemoved ?? []` iterate `{name}`
//!   references; recorded selection = adds minus removes in transcript
//!   order;
//! - `messages.length === 0` drives the eager/auto decision for empty
//!   sessions.
//!
//! The host-backed implementation reads the session projection the session
//! host pushes (`session_ctx.rs` → `buildSessionProjection()`); a missing
//! projection (headless / `--no-session` hosts) degrades to the `entries`
//! snapshot, which IS the projected message list on session hosts.

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::json;

fn test_config() -> ExtensionConfig {
    ExtensionConfig {
        cwd: std::env::temp_dir(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn new_runtime() -> ExtensionRuntime {
    ExtensionRuntime::new(test_config())
        .await
        .expect("runtime construction")
}

/// The pi-subagents consumption sequence, verbatim shape: replay the
/// recorded tool selection out of `buildSessionContext().messages` and
/// report the decision inputs. `snapshot` stubs the host session snapshot
/// the way the session host pushes it (context_fns.rs
/// `getSessionManagerJson`); the stub lives in the SAME module as the probe
/// because only the probe module is seeded into the runtime.
fn probe_extension(snapshot: &str) -> String {
    // Concatenation, not format!: the JS body is full of raw `{`/`}`.
    let mut source = String::from(
        r#"
globalThis.__pir_host.getSessionManagerJson = () => JSON.stringify("#,
    );
    source.push_str(snapshot);
    source.push_str(
        r#");
export default (pi) => {
    pi.on("session_start", (_event, ctx) => {
        const sm = ctx.sessionManager;
        if (typeof sm.buildSessionContext !== "function") {
            globalThis.__probe = { error: "buildSessionContext is not a function" };
            return;
        }
        const messages = sm.buildSessionContext().messages;
        if (!Array.isArray(messages)) {
            globalThis.__probe = { error: "messages is not an array" };
            return;
        }
        // hasNativeToolSelection (tool-activation.js): system messages with
        // a recorded toolsAdded/toolsRemoved delta.
        const hasNative = messages.some((message) => message.role === "system"
            && (Object.hasOwn(message, "toolsAdded")
                || Object.hasOwn(message, "toolsRemoved")));
        const recorded = new Set();
        for (const message of messages) {
            if (message.role !== "system") continue;
            for (const tool of message.toolsRemoved ?? []) recorded.delete(tool.name);
            for (const tool of message.toolsAdded ?? []) recorded.add(tool.name);
        }
        // "auto" goes eager only for an empty session.
        const eager = messages.length === 0;
        globalThis.__probe = {
            count: messages.length,
            hasNative,
            recorded: [...recorded].sort(),
            eager,
            keys: Object.keys(sm.buildSessionContext()).sort(),
        };
    });
};
"#,
    );
    source
}

async fn probe_with_snapshot(snapshot: &str) -> serde_json::Value {
    let rt = new_runtime().await;
    rt.seed_module("pir-ext:probe.ts", &probe_extension(snapshot))
        .expect("seed");
    rt.seeded_load("pir-ext:probe.ts").await.expect("load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt.emit("session_start", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    rt.eval_json("JSON.stringify(globalThis.__probe)")
        .await
        .expect("eval")
}

/// A mid-session snapshot whose projection carries the upstream system
/// messages: the loader was added then the subagent was added, and a later
/// system message removed the loader again.
#[tokio::test(flavor = "current_thread")]
async fn replay_recorded_tool_selection_from_projection() {
    let probe = probe_with_snapshot(
        r#"
        {
            "leafId": "m2",
            "projection": {
                "messages": [
                    { "role": "system", "content": [{ "type": "text", "text": "loadout" }],
                      "toolsAdded": [{ "name": "subagents_enable" }] },
                    { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                    { "role": "system", "content": [{ "type": "text", "text": "loadout" }],
                      "toolsAdded": [{ "name": "subagent" }] },
                    { "role": "system", "content": [{ "type": "text", "text": "loadout" }],
                      "toolsRemoved": [{ "name": "subagents_enable" }] }
                ]
            }
        }
        "#,
    )
    .await;
    assert_eq!(probe["error"], serde_json::Value::Null, "{probe}");
    assert_eq!(probe["count"], 4, "{probe}");
    // A toolsAdded/toolsRemoved delta exists → native tool selection.
    assert_eq!(probe["hasNative"], true, "{probe}");
    // adds minus removes, in transcript order: subagents_enable removed.
    assert_eq!(probe["recorded"], json!(["subagent"]), "{probe}");
    // Non-empty session → not eager.
    assert_eq!(probe["eager"], false, "{probe}");
    assert_eq!(probe["keys"], json!(["messages"]), "{probe}");
}

/// System messages WITHOUT a recorded delta must fail `Object.hasOwn` — the
/// host serialization omits absent fields (skip_serializing_if), and the
/// probe's recorded set stays empty.
#[tokio::test(flavor = "current_thread")]
async fn system_messages_without_delta_are_not_native_selections() {
    let probe = probe_with_snapshot(
        r#"
        {
            "projection": {
                "messages": [
                    { "role": "system", "content": [{ "type": "text", "text": "plain" }] },
                    { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                    { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] }
                ]
            }
        }
        "#,
    )
    .await;
    assert_eq!(probe["error"], serde_json::Value::Null, "{probe}");
    assert_eq!(probe["count"], 3, "{probe}");
    assert_eq!(probe["hasNative"], false, "{probe}");
    assert_eq!(probe["recorded"], json!([]), "{probe}");
    assert_eq!(probe["eager"], false, "{probe}");
}

/// Empty session (empty projection) → the eager/auto decision input.
#[tokio::test(flavor = "current_thread")]
async fn empty_session_reports_zero_messages() {
    let probe = probe_with_snapshot(
        r#"
        { "leafId": null, "projection": { "messages": [] } }
        "#,
    )
    .await;
    assert_eq!(probe["error"], serde_json::Value::Null, "{probe}");
    assert_eq!(probe["count"], 0, "{probe}");
    assert_eq!(probe["hasNative"], false, "{probe}");
    assert_eq!(probe["eager"], true, "{probe}");
}

/// Headless / `--no-session` hosts push no projection: the `entries`
/// snapshot (projected context entries on session hosts, raw history on
/// print hosts) degrades to the message list.
#[tokio::test(flavor = "current_thread")]
async fn missing_projection_falls_back_to_entries() {
    let probe = probe_with_snapshot(
        r#"
        {
            "entries": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] }
            ]
        }
        "#,
    )
    .await;
    assert_eq!(probe["error"], serde_json::Value::Null, "{probe}");
    assert_eq!(probe["count"], 2, "{probe}");
    assert_eq!(probe["eager"], false, "{probe}");
}

/// No snapshot at all (the runtime's initial state) degrades to an empty
/// message list rather than throwing.
#[tokio::test(flavor = "current_thread")]
async fn missing_snapshot_degrades_to_empty_messages() {
    let probe = probe_with_snapshot("{}").await;
    assert_eq!(probe["error"], serde_json::Value::Null, "{probe}");
    assert_eq!(probe["count"], 0, "{probe}");
    assert_eq!(probe["eager"], true, "{probe}");
}
