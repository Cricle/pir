//! Regression tests for the extension-runtime audit fixes:
//!
//! - P1-1/P2-4 `user_bash` handler errors and invalid shapes abort instead of
//!   falling back to local execution, with the full upstream result-shape
//!   check.
//! - P1-2 `session_before_fork/compact/tree` cancel short-circuits the whole
//!   dispatch; a later handler cannot overwrite an earlier `cancel: true`.
//! - P1-3 owner-tagged interrupt deadlines: a concurrent call's disarm cannot
//!   leave another call's synchronous JS loop uninterruptible.
//! - P2-5 `format: "uri"` / `"ipv6"` validators work without `new URL`.
//! - P2-6 load discard only deletes tool metadata the failing extension owns.
//! - P2-7 `message_end` returning `message: null` is isolated, not fatal.
//! - P2-8 boundary events thread earlier handlers' `entries`/`continue`.
//! - P2-9 `project_trust`: first decided handler wins, "undecided" falls
//!   through.
//! - P2-10 `before_agent_start` accumulates messages and chains the forced
//!   system prompt into the event and `ctx.getSystemPrompt`.
//! - P2-11 `context` in-place `event.messages` mutation is honored.
//! - P2-12 the handler list is snapshotted before dispatch.
//! - P2-13 `pi.getFlag` resolves the declared default during load.
//! - P2-14 `console.log`/`warn`/`error` are routed to the notify path.
//! - P2-15 schema validator equality is key-order insensitive.
//! - P2-16 a JS command named `reload` is not shadowed by the builtin.
//! - P2-17 the notification queue is capped (ToolUpdates coalesced).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{
    dispatch_project_trust, dispatch_user_bash, take_session_command_actions, AgentSession,
    AgentSessionConfig, SessionCommandAction, UserBashOutcome,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::load_pi_environment;
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, ThinkingLevel,
};
use pir::config::settings::SettingsManager;
use pir::config::trust::ProjectTrustStore;
use pir::extensions::project_trust::{
    clear_recorded_decisions, override_or_extension_decision, pin_decision, resolve_project_trust,
    resolved_decision, ProjectTrustDecision,
};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::skills::SkillInfo;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_dir(test_name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-ext-fixes-{test_name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn handle_config(cwd: &Path, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui,
        model: Some("fixes-model".to_string()),
        session_id: Some("fixes-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn seed_and_load(handle: &ExtensionHandle, dir: &Path, name: &str, source: &str) {
    let path = dir.join(name);
    std::fs::write(&path, source).expect("write fixture");
    let result = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
}

async fn probe(handle: &ExtensionHandle, code: &str) -> Value {
    handle
        .eval_json(&format!("JSON.stringify({code})"))
        .await
        .expect("probe eval")
}

// ---------------------------------------------------------------------------
// P1-1 + P2-4: user_bash abort semantics and the full result-shape check
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn user_bash_throwing_handler_aborts_instead_of_local_execution() {
    let dir = fixture_dir("ub-throw");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("user_bash", () => { throw new Error("bash handler exploded"); });
        }
        "#,
    )
    .await;

    match dispatch_user_bash(&handle, "ls -la", false).await {
        UserBashOutcome::Aborted => {}
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn user_bash_invalid_shapes_are_rejected_like_upstream() {
    let dir = fixture_dir("ub-shape");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    // Each handler returns a shape upstream rejects (runner.ts:140-164):
    // - missing exitCode / cancelled / truncated on result
    // - operations without an exec function
    // - both operations and result set
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            const cases = [
                { result: { output: "x" } },
                { result: { output: "x", exitCode: 0 } },
                { result: { output: "x", exitCode: 0, cancelled: false } },
                { result: { output: 1, exitCode: 0, cancelled: false, truncated: false } },
                { operations: {} },
                { operations: { exec: "not-a-function" }, },
                { operations: { exec: () => {} }, result: { output: "x", exitCode: 0, cancelled: false, truncated: false } },
                "just a string",
            ];
            let index = 0;
            pi.on("user_bash", () => {
                const current = cases[index % cases.length];
                index += 1;
                globalThis.__lastCase = index;
                return current;
            });
        }
        "#,
    )
    .await;

    for case in 0..8 {
        match dispatch_user_bash(&handle, "ls", false).await {
            UserBashOutcome::Aborted => {}
            other => panic!("case {case}: expected Aborted, got {other:?}"),
        }
        let seen = probe(&handle, "globalThis.__lastCase").await;
        assert_eq!(seen, json!(case + 1));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn user_bash_valid_results_still_reach_the_host() {
    let dir = fixture_dir("ub-valid");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("user_bash", (event) => {
                if (event.command === "handled") {
                    return {
                        result: { output: "out", exitCode: 3, cancelled: false, truncated: false },
                    };
                }
                if (event.command === "ops") {
                    return { operations: { exec: async () => {} } };
                }
                return undefined;
            });
        }
        "#,
    )
    .await;

    match dispatch_user_bash(&handle, "handled", false).await {
        UserBashOutcome::Handled { output, is_error } => {
            assert_eq!(output, "out");
            assert!(!is_error);
        }
        other => panic!("expected Handled, got {other:?}"),
    }
    match dispatch_user_bash(&handle, "ops", false).await {
        UserBashOutcome::Operations { operations } => {
            // The shape check (exec is a function) runs inside QuickJS; the
            // JSON boundary to the host drops the function itself.
            assert!(operations.is_object());
        }
        other => panic!("expected Operations, got {other:?}"),
    }
    // No decision: local execution.
    match dispatch_user_bash(&handle, "plain", false).await {
        UserBashOutcome::Local => {}
        other => panic!("expected Local, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P1-2: session_before_fork/compact/tree cancel short-circuit
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn session_before_cancel_short_circuits_all_variants() {
    let dir = fixture_dir("sb-cancel");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            for (const event of ["session_before_fork", "session_before_compact", "session_before_tree"]) {
                pi.on(event, () => {
                    globalThis[`__${event}_first`] = true;
                    return { cancel: true };
                });
                pi.on(event, () => {
                    // Must never run: the earlier cancel short-circuits (P1-2).
                    globalThis[`__${event}_second`] = true;
                    return { cancel: false };
                });
            }
        }
        "#,
    )
    .await;

    for event in [
        "session_before_fork",
        "session_before_compact",
        "session_before_tree",
    ] {
        let mut payload = json!({ "type": event });
        let outcome = handle.emit(event, &mut payload).await.expect("emit");
        assert_eq!(outcome.result, json!({ "cancel": true }), "{event}");
        let second = probe(&handle, &format!("globalThis.__{event}_second ?? null")).await;
        assert_eq!(second, json!(null), "{event}: later handler must not run");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn session_before_tree_non_cancel_fields_still_merge() {
    let dir = fixture_dir("sb-merge");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("session_before_tree", () => ({ label: "keep" }));
            pi.on("session_before_tree", () => ({ customInstructions: "summarize well" }));
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "session_before_tree" });
    let outcome = handle
        .emit("session_before_tree", &mut payload)
        .await
        .expect("emit");
    assert_eq!(outcome.result["label"], json!("keep"));
    assert_eq!(
        outcome.result["customInstructions"],
        json!("summarize well")
    );
}

// ---------------------------------------------------------------------------
// P1-3: owner-tagged interrupt deadlines under concurrent timed calls
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn concurrent_disarm_does_not_cancel_another_calls_interrupt_deadline() {
    let dir = fixture_dir("p1-3-deadline");
    let handle = ExtensionHandle::start(handle_config(&dir, true)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "gate",
                parameters: { type: "object", properties: {} },
                execute: async (_id, _params, _signal, _onUpdate, ctx) => {
                    // Suspend on a host promise, then run away synchronously.
                    await ctx.ui.select("gate", ["go"]);
                    while (true) { /* runaway */ }
                },
            });
            pi.registerTool({
                name: "quick",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
            });
        }
        "#,
    )
    .await;
    handle.set_call_timeout(Duration::from_millis(2000));

    // Start the gated call; it suspends on the dialog host promise.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let gated = handle.clone();
    tokio::task::spawn(async move {
        let result = gated.call_tool("gate", "call-a", &json!({})).await;
        let _ = reply_tx.send(result);
    });

    let mut dialog_id = None;
    for _ in 0..200 {
        let requests = handle.take_notifications(|notification| {
            matches!(notification, Notification::DialogRequest { .. })
        });
        if let Some(Notification::DialogRequest { id, .. }) = requests.into_iter().next() {
            dialog_id = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let dialog_id = dialog_id.expect("gated call must reach the dialog");
    let dialog_id = dialog_id.to_string();

    // A second timed call runs to completion while the first is suspended.
    // With a shared deadline slot its disarm would cancel the first call's
    // deadline; with owner-tagged entries it only removes its own (P1-3).
    let quick = handle
        .call_tool("quick", "call-b", &json!({}))
        .await
        .expect("quick call completes");
    assert_eq!(quick.content[0].text, "ok");

    // Release the gated call into its runaway synchronous loop: its own
    // deadline must still interrupt it (the actor thread must not hang).
    handle.respond_dialog(&dialog_id, json!("go"));
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(30), reply_rx)
        .await
        .expect("gated call must settle - the actor thread must not hang");
    let result = result.expect("reply channel");
    assert!(result.is_err(), "runaway gated call must fail: {result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "interrupt deadline must fire promptly, took {:?}",
        started.elapsed()
    );

    // The runtime stays usable after the interrupt.
    let one = handle.eval_json("JSON.stringify(1)").await.expect("eval");
    assert_eq!(one, json!(1));
}

// ---------------------------------------------------------------------------
// P2-5: format uri / ipv6 validators
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn schema_format_uri_and_ipv6_validate_for_real() {
    let dir = fixture_dir("p2-5-formats");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        "export default function (pi) { void pi; }",
    )
    .await;

    // uri: `new URL` does not exist in QuickJS; a real RFC 3986 check runs.
    let ok = probe(
        &handle,
        "__pir_schemaValidate({ type: \"string\", format: \"uri\" }, \"https://example.com/x?y=1#z\")",
    )
    .await;
    assert_eq!(ok["valid"], json!(true), "{ok}");
    let ok = probe(
        &handle,
        "__pir_schemaValidate({ type: \"string\", format: \"uri\" }, \"mailto:user@example.com\")",
    )
    .await;
    assert_eq!(ok["valid"], json!(true), "{ok}");
    let bad = probe(
        &handle,
        "__pir_schemaValidate({ type: \"string\", format: \"uri\" }, \"not a uri at all\")",
    )
    .await;
    assert_eq!(bad["valid"], json!(false), "{bad}");
    let bad = probe(
        &handle,
        "__pir_schemaValidate({ type: \"string\", format: \"uri\" }, \"no scheme\")",
    )
    .await;
    assert_eq!(bad["valid"], json!(false), "{bad}");

    // ipv6: ":" alone must not pass; real group structure is checked.
    for (value, expected) in [
        ("::1", true),
        ("1:2:3:4:5:6:7:8", true),
        ("2001:db8::8a2e:370:7334", true),
        ("::ffff:192.168.1.1", true),
        ("1:2:3:4:5:6:1.2.3.4", true),
        (":", false),
        ("::::", false),
        ("12345::", false),
        ("1:2:3:4:5:6:7:8:9", false),
        ("1:2:3:4:5:6:7:1.2.3.4", false),
        ("gggg::1", false),
    ] {
        let out = probe(
            &handle,
            &format!("__pir_schemaValidate({{ type: \"string\", format: \"ipv6\" }}, {value:?})"),
        )
        .await;
        assert_eq!(out["valid"], json!(expected), "ipv6 {value}: {out}");
    }
}

// ---------------------------------------------------------------------------
// P2-6: load discard only deletes tool metadata the failing extension owns
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn failed_extension_discard_keeps_other_extensions_tool_metadata() {
    let dir = fixture_dir("p2-6-discard");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    let path_a = dir.join("owner.ts");
    std::fs::write(
        &path_a,
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "shared-tool",
                description: "from the surviving owner",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [] }),
            });
        }
        "#,
    )
    .expect("write owner fixture");
    let path_b = dir.join("later-fails.ts");
    std::fs::write(
        &path_b,
        r#"
        export default function (pi) {
            // Re-registers the first-wins name and adds its own tool, then
            // fails: the discard must only remove what it owns (P2-6).
            pi.registerTool({
                name: "shared-tool",
                description: "from the failing later extension",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [] }),
            });
            pi.registerTool({
                name: "b-only",
                description: "owned by the failing extension",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [] }),
            });
            throw new Error("intentional failure");
        }
        "#,
    )
    .expect("write failing fixture");

    let result = handle.load(&[path_a, path_b]).await.expect("load");
    assert_eq!(result.loaded.len(), 1);
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].error.contains("intentional failure"));

    let defs = handle.tool_defs();
    let shared = defs
        .iter()
        .find(|def| def.name == "shared-tool")
        .expect("surviving first-wins metadata must not be deleted by the failed discard");
    assert_eq!(shared.description, "from the surviving owner");
    assert!(
        !defs.iter().any(|def| def.name == "b-only"),
        "the failing extension's own tool must be cleaned up: {defs:?}"
    );
}

// ---------------------------------------------------------------------------
// P2-7: message_end returning message: null is isolated, not fatal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn message_end_null_message_does_not_crash_the_dispatch() {
    let dir = fixture_dir("p2-7-msgend");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("message_end", () => ({ message: null }));
            pi.on("message_end", (event) => {
                globalThis.__sawRole = event.message.role;
                return undefined;
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "message_end",
        "message": { "role": "assistant", "content": "original" },
    });
    let outcome = handle
        .emit("message_end", &mut payload)
        .await
        .expect("a null message must be treated as no replacement, not crash the dispatch");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result, Value::Null);
    let saw = probe(&handle, "globalThis.__sawRole").await;
    assert_eq!(saw, json!("assistant"));
}

// ---------------------------------------------------------------------------
// P2-8: boundary events thread earlier handlers' entries/continue
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn boundary_events_thread_entries_and_continue_across_handlers() {
    let dir = fixture_dir("p2-8-boundary");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("turn_end", () => ({
                entries: [{ type: "custom", customType: "first-note", data: {} }],
            }));
            pi.on("turn_end", (event) => {
                globalThis.__sawEntries = event.entries.length;
                globalThis.__sawContinue = event.continue;
                return { continue: true };
            });
            pi.on("agent_before_settle", (event) => {
                globalThis.__settleSawEntries = event.entries.length;
                globalThis.__settleSawContinue = event.continue;
                return undefined;
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "turn_end", "entries": [], "continue": false });
    let outcome = handle.emit("turn_end", &mut payload).await.expect("emit");
    assert_eq!(outcome.result["continue"], json!(true));
    assert_eq!(outcome.result["entries"].as_array().map(Vec::len), Some(1));
    let saw = probe(
        &handle,
        "[globalThis.__sawEntries, globalThis.__sawContinue]",
    )
    .await;
    assert_eq!(
        saw,
        json!([1, false]),
        "second handler sees the first draft"
    );

    // agent_before_settle chains too: it sees the turn_end drafts.
    let mut payload = json!({ "type": "agent_before_settle", "entries": [], "continue": false });
    let _ = handle
        .emit("agent_before_settle", &mut payload)
        .await
        .expect("emit");
    let settle = probe(
        &handle,
        "[globalThis.__settleSawEntries, globalThis.__settleSawContinue]",
    )
    .await;
    // A fresh dispatch restarts the boundary chain from the payload.
    assert_eq!(settle, json!([0, false]));
}

// ---------------------------------------------------------------------------
// P2-9: project_trust first decided wins, "undecided" falls through
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn project_trust_first_decided_handler_wins() {
    let dir = fixture_dir("p2-9-trust");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "undecided" }));
            pi.on("project_trust", () => {
                globalThis.__secondRan = true;
                return { trusted: "no" };
            });
            pi.on("project_trust", () => {
                globalThis.__thirdRan = true;
                return { trusted: "yes", remember: true };
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "project_trust", "cwd": dir.display().to_string() });
    let outcome = handle
        .emit("project_trust", &mut payload)
        .await
        .expect("emit");
    // "undecided" is not a decision; the first decided result wins and stops
    // the dispatch (upstream emitProjectTrustEvent).
    assert_eq!(outcome.result["trusted"], json!("no"));
    assert!(outcome.result.get("remember").is_none());
    let third = probe(&handle, "globalThis.__thirdRan ?? null").await;
    assert_eq!(third, json!(null), "dispatch must stop after the decision");
}

// ---------------------------------------------------------------------------
// P2-10: before_agent_start accumulates messages and chains the system prompt
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn before_agent_start_accumulates_messages_and_chains_system_prompt() {
    let dir = fixture_dir("p2-10-bas");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("before_agent_start", () => ({
                systemPrompt: "forced prompt",
                message: { customType: "a", content: [{ type: "text", text: "m1" }] },
            }));
            pi.on("before_agent_start", (event, ctx) => {
                globalThis.__sawEventPrompt = event.systemPrompt;
                globalThis.__sawCtxPrompt = ctx.getSystemPrompt();
                return { message: { customType: "b", content: [{ type: "text", text: "m2" }] } };
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "before_agent_start",
        "prompt": "hello",
        "systemPrompt": "original prompt",
    });
    let outcome = handle
        .emit("before_agent_start", &mut payload)
        .await
        .expect("emit");
    assert_eq!(outcome.result["systemPrompt"], json!("forced prompt"));
    let messages = outcome.result["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "every handler's message is accumulated");
    let saw = probe(
        &handle,
        "[globalThis.__sawEventPrompt, globalThis.__sawCtxPrompt]",
    )
    .await;
    assert_eq!(
        saw,
        json!(["forced prompt", "forced prompt"]),
        "later handlers see the forced prompt in the event and via ctx.getSystemPrompt"
    );
}

// ---------------------------------------------------------------------------
// P2-11: context in-place event.messages mutation is honored
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn context_in_place_mutation_becomes_the_new_messages() {
    let dir = fixture_dir("p2-11-context");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("context", (event) => {
                globalThis.__len1 = event.messages.length;
                event.messages.push({ role: "user", content: "in-place" });
                return undefined;
            });
            pi.on("context", (event) => {
                globalThis.__len2 = event.messages.length;
                globalThis.__last = event.messages[event.messages.length - 1].content;
                return undefined;
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "context",
        "messages": [{ "role": "user", "content": "base" }],
    });
    let outcome = handle.emit("context", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let messages = outcome.result["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "the in-place push must be reported back");
    assert_eq!(messages[1]["content"], json!("in-place"));
    let saw = probe(
        &handle,
        "[globalThis.__len1, globalThis.__len2, globalThis.__last]",
    )
    .await;
    assert_eq!(saw, json!([1, 2, "in-place"]));
}

// ---------------------------------------------------------------------------
// P2-12: the handler list is snapshotted before dispatch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn handler_registered_during_dispatch_joins_only_the_next_round() {
    let dir = fixture_dir("p2-12-snapshot");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("session_start", () => {
                globalThis.__rounds = (globalThis.__rounds ?? 0) + 1;
                if (globalThis.__rounds === 1) {
                    pi.on("session_start", () => {
                        globalThis.__lateRegisteredRan = true;
                    });
                }
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "session_start", "reason": "startup" });
    let _ = handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    let late = probe(&handle, "globalThis.__lateRegisteredRan ?? null").await;
    assert_eq!(
        late,
        json!(null),
        "a handler registered during dispatch must not join the current round"
    );

    let mut payload = json!({ "type": "session_start", "reason": "startup" });
    let _ = handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    let late = probe(&handle, "globalThis.__lateRegisteredRan ?? null").await;
    assert_eq!(late, json!(true), "the registered handler runs next round");
}

// ---------------------------------------------------------------------------
// P2-13: getFlag resolves the declared default during load
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn get_flag_returns_declared_default_during_load() {
    let dir = fixture_dir("p2-13-flag");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerFlag("load-flag", { type: "string", default: "from-default" });
            globalThis.__flagDuringLoad = pi.getFlag("load-flag");
            pi.registerFlag("no-default", { type: "boolean" });
            globalThis.__flagNoDefault = pi.getFlag("no-default");
        }
        "#,
    )
    .await;

    let during_load = probe(&handle, "globalThis.__flagDuringLoad").await;
    assert_eq!(during_load, json!("from-default"));
    let no_default = probe(&handle, "globalThis.__flagNoDefault ?? null").await;
    assert_eq!(no_default, json!(null));

    // After commit the host registry answers (CLI value > default).
    assert_eq!(
        handle.get_flag("load-flag"),
        Some(pir::extensions::FlagValue::Str("from-default".to_string()))
    );
}

// ---------------------------------------------------------------------------
// P2-14: console is routed to the notify path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn console_shim_routes_to_the_notify_path() {
    let dir = fixture_dir("p2-14-console");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.on("session_start", () => {
                console.log("hello-console", { a: 1 });
                console.warn("warn-console");
                console.error("error-console");
                console.info("info-console");
                console.debug("debug-console");
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "session_start", "reason": "startup" });
    let outcome = handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let notifications = handle.drain_notifications();
    let find = |needle: &str| {
        notifications
            .iter()
            .find_map(|notification| match notification {
                Notification::Notify { message, level } if message.contains(needle) => {
                    Some(message.clone()).map(|message| (message, level.clone()))
                }
                _ => None,
            })
    };
    let (message, level) = find("hello-console").expect("console.log captured");
    // Node formats extra console arguments with util.inspect (`{ a: 1 }`),
    // not JSON (`{"a":1}`); the console wave aligned the formatting.
    assert!(message.contains("{ a: 1 }"), "{message}");
    assert_eq!(level, "debug");
    let (_, level) = find("warn-console").expect("console.warn captured");
    assert_eq!(level, "warning");
    let (_, level) = find("error-console").expect("console.error captured");
    assert_eq!(level, "error");
    let (_, level) = find("info-console").expect("console.info captured");
    assert_eq!(level, "debug");
    let (_, level) = find("debug-console").expect("console.debug captured");
    assert_eq!(level, "debug");
}

// ---------------------------------------------------------------------------
// P2-15: schema validator equality is key-order insensitive
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn schema_validator_equality_ignores_key_order() {
    let dir = fixture_dir("p2-15-deepequal");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        "export default function (pi) { void pi; }",
    )
    .await;

    // const with reordered keys must be equal.
    let out = probe(
        &handle,
        "__pir_schemaValidate({ const: { a: 1, b: 2 } }, { b: 2, a: 1 })",
    )
    .await;
    assert_eq!(out["valid"], json!(true), "{out}");

    // Nested reordering deep in the structure.
    let out = probe(
        &handle,
        "__pir_schemaValidate({ const: { outer: { x: [1, { y: 2, z: 3 }] } } }, { outer: { x: [1, { z: 3, y: 2 }] } })",
    )
    .await;
    assert_eq!(out["valid"], json!(true), "{out}");

    // enum members compared deeply too.
    let out = probe(
        &handle,
        "__pir_schemaValidate({ enum: [{ p: 1, q: 2 }, [1, 2]] }, [1, 2])",
    )
    .await;
    assert_eq!(out["valid"], json!(true), "{out}");

    // Genuine mismatches still fail (extra key, different value, shorter list).
    for (schema, value) in [
        (
            json!({ "const": { "a": 1, "b": 2 } }),
            json!({ "a": 1, "b": 3 }),
        ),
        (json!({ "const": { "a": 1 } }), json!({ "a": 1, "b": 2 })),
        (json!({ "enum": [{ "a": 1 }] }), json!([{ "a": 2 }])),
    ] {
        let out = probe(
            &handle,
            &format!("__pir_schemaValidate({}, {})", schema, value),
        )
        .await;
        assert_eq!(out["valid"], json!(false), "{schema} vs {value}: {out}");
    }
}

// ---------------------------------------------------------------------------
// P2-16: a JS command named reload is not shadowed by the builtin
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn js_reload_command_wins_over_the_builtin_reload() {
    let dir = fixture_dir("p2-16-reload");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerCommand("reload", {
                description: "extension reload command",
                handler: async () => {
                    globalThis.__jsReloadRan = true;
                },
            });
        }
        "#,
    )
    .await;

    let outcome = handle
        .dispatch_command("reload", "")
        .await
        .expect("dispatch");
    assert!(outcome.ok, "{:?}", outcome.error);
    // If the builtin reload had run, the runtime would have been rebuilt and
    // this global would be gone; only the JS handler sets it.
    let ran = probe(&handle, "globalThis.__jsReloadRan ?? null").await;
    assert_eq!(ran, json!(true), "the JS reload command must run");
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_reload_still_runs_without_a_js_command() {
    let dir = fixture_dir("p2-16-builtin");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerCommand("other-command", { handler: async () => {} });
        }
        "#,
    )
    .await;

    // No JS command named reload: the builtin in-process loader re-run wins.
    let outcome = handle
        .dispatch_command("reload", "")
        .await
        .expect("dispatch");
    assert!(outcome.ok, "{:?}", outcome.error);
    // The reload re-ran the factory in a fresh runtime and emitted the
    // lifecycle events; the extension command is still registered.
    assert!(handle
        .command_defs()
        .iter()
        .any(|command| command.name == "other-command"));
}

// ---------------------------------------------------------------------------
// Audit §4 #5: `withSession` callbacks ride a runtime token and run with the
// REPLACEMENT session's context (upstream ReplacedSessionContext)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn command_context_with_session_callbacks_ride_a_token_and_run() {
    let dir = fixture_dir("with-session");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerCommand("try-new", {
                handler: async (_args, ctx) => {
                    await ctx.newSession({
                        withSession: async (replaced) => {
                            globalThis.__with_session_runs = (globalThis.__with_session_runs ?? 0) + 1;
                            globalThis.__with_session_id = replaced.sessionManager.getSessionId();
                        },
                    });
                },
            });
            pi.registerCommand("bad-with-session", {
                handler: async (_args, ctx) => {
                    await ctx.newSession({ withSession: "not-a-function" });
                },
            });
        }
        "#,
    )
    .await;

    // The host pushes the live session snapshot the command ctx (and the
    // later callback ctx) is built from.
    let snapshot = pir::extensions::SessionContextSnapshot {
        session_id: Some("command-session".to_owned()),
        ..Default::default()
    };
    handle.update_session_context(snapshot);

    // A non-function is a caller bug: rejected at the call site, before any
    // host action is recorded (and without leaking a callback token).
    let outcome = handle
        .dispatch_command("bad-with-session", "")
        .await
        .expect("dispatch");
    assert!(!outcome.ok, "a non-function withSession must be rejected");
    let error = outcome.error.unwrap_or_default();
    assert!(
        error.contains("withSession must be a function"),
        "the error must name the invalid option: {error}"
    );
    assert!(
        take_session_command_actions().is_empty(),
        "a rejected call must not record an action"
    );

    // A real callback is accepted: the JS surface no longer throws, and the
    // recorded action carries the callback's token.
    let outcome = handle
        .dispatch_command("try-new", "")
        .await
        .expect("dispatch");
    assert!(outcome.ok, "{:?}", outcome.error);
    let actions = take_session_command_actions();
    assert_eq!(actions.len(), 1, "one action recorded, got {actions:?}");
    let SessionCommandAction::NewSession { with_session, .. } = &actions[0] else {
        panic!("expected a NewSession action, got {:?}", actions[0]);
    };
    let token = with_session.expect("the withSession token rides the action");

    // The host pushes the REPLACEMENT session's snapshot, then calls back.
    // The callback's fresh ctx must read the new session, not the one the
    // command ran in.
    let snapshot = pir::extensions::SessionContextSnapshot {
        session_id: Some("replacement-session".to_owned()),
        ..Default::default()
    };
    handle.update_session_context(snapshot);
    handle
        .call_session_callback(token)
        .await
        .expect("the host-side callback bridge runs the registered function");
    assert_eq!(
        probe(&handle, "globalThis.__with_session_runs").await,
        json!(1),
        "the callback ran exactly once"
    );
    assert_eq!(
        probe(&handle, "globalThis.__with_session_id").await,
        json!("replacement-session"),
        "the callback's ctx is bound to the replacement session"
    );

    // One-shot: a repeated invocation is an explicit error, never a silent
    // no-op (the token was consumed).
    let second = handle.call_session_callback(token).await;
    assert!(
        second.is_err(),
        "a consumed withSession token must report the failure"
    );
    assert!(
        second.unwrap_err().to_string().contains("not registered"),
        "the error must say the token is gone"
    );
}

// ---------------------------------------------------------------------------
// Audit §4 #29: a JS tool's declared label reaches the native AgentTool
// contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn js_tool_label_reaches_the_native_agent_tool_contract() {
    let dir = fixture_dir("js-tool-label");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "labelled",
                label: "Labelled Tool",
                description: "has a label",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
            });
        }
        "#,
    )
    .await;

    let mut registry = ToolRegistry::new();
    pir::agent::tool_execution::merge_extension_tools(&mut registry, &handle);
    let tool = registry.get("labelled").expect("registered extension tool");
    assert_eq!(
        tool.label(),
        "Labelled Tool",
        "the extension's declared label is the native AgentTool label"
    );
    // A JS tool's `prepareArguments` stays inside QuickJS (`__pir_callTool`
    // runs it before its own schema check, audit #9), so the host preflight
    // does not claim the native capability.
    assert!(!tool.prepares_arguments());
    // Upstream `ToolDefinition` has no replay field: the durable default is
    // "never".
    assert_eq!(tool.replay(), pir::agent::tool_execution::ToolReplay::Never);
}

// ---------------------------------------------------------------------------
// P2-17: the notification queue is capped, ToolUpdates coalesced
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn notification_queue_is_capped_and_coalesces_tool_updates() {
    let dir = fixture_dir("p2-17-queue");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "ext.ts",
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "spam",
                parameters: { type: "object", properties: {} },
                execute: async (_id, _params, _signal, onUpdate, ctx) => {
                    for (let i = 0; i < 3000; i += 1) {
                        onUpdate({ index: i });
                    }
                    ctx.ui.notify("side-effect-kept", "info");
                    return { content: [{ type: "text", text: "done" }] };
                },
            });
        }
        "#,
    )
    .await;

    let result = handle
        .call_tool("spam", "call-spam", &json!({}))
        .await
        .expect("spam tool runs");
    assert_eq!(result.content[0].text, "done");

    let notifications = handle.drain_notifications();
    let updates: Vec<&Notification> = notifications
        .iter()
        .filter(|notification| matches!(notification, Notification::ToolUpdate { .. }))
        .collect();
    assert!(
        updates.len() <= 1024,
        "the queue must be capped, got {} ToolUpdates",
        updates.len()
    );
    assert!(!updates.is_empty(), "recent updates must be kept");
    // The newest partial for the call survives the coalescing.
    let last = match updates.last().expect("at least one update") {
        Notification::ToolUpdate { partial, .. } => partial.clone().expect("partial"),
        _ => unreachable!("filtered above"),
    };
    assert_eq!(last["index"], json!(2999));
    // Non-ToolUpdate side effects are never dropped.
    assert!(notifications
        .iter()
        .any(|notification| matches!(notification, Notification::Notify { message, .. } if message == "side-effect-kept")));
}

// ---------------------------------------------------------------------------
// Audit §4 #2: the extension `project_trust` decision actually applies
// ---------------------------------------------------------------------------

/// Serializes the process env pins of this suite (g_extensions has no shared
/// `common` module, so the lock is local and only this test mutates these
/// variables).
/// Serializes tests that mutate `PIR_TRUST_PROJECT`. These tests hold the
/// guard across their async body on purpose (the environment must stay pinned
/// for the whole scenario), hence the per-test `await_holding_lock` allow.
static TRUST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Panic-safe pin of `PIR_AGENT_DIR` with `PIR_TRUST_PROJECT` unset.
struct TrustEnvGuard {
    trust: Option<std::ffi::OsString>,
    agent: Option<std::ffi::OsString>,
}

impl TrustEnvGuard {
    fn new(agent_dir: &Path) -> Self {
        let guard = Self {
            trust: std::env::var_os("PIR_TRUST_PROJECT"),
            agent: std::env::var_os("PIR_AGENT_DIR"),
        };
        std::env::remove_var("PIR_TRUST_PROJECT");
        std::env::set_var("PIR_AGENT_DIR", agent_dir);
        guard
    }
}

impl Drop for TrustEnvGuard {
    fn drop(&mut self) {
        match self.trust.take() {
            Some(value) => std::env::set_var("PIR_TRUST_PROJECT", value),
            None => std::env::remove_var("PIR_TRUST_PROJECT"),
        }
        match self.agent.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
        clear_recorded_decisions();
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn project_trust_extension_decision_changes_the_trust_result() {
    let dir = fixture_dir("project-trust-effect");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(dir.join(".pi")).expect("project dir");
    // A trust-gated project resource: this only loads when the scope is
    // trusted.
    std::fs::write(
        dir.join(".pi/settings.json"),
        r#"{"theme":"project-theme"}"#,
    )
    .expect("project settings");

    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "trust.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "yes", remember: true }));
        }
        "#,
    )
    .await;

    let cwd_text = dir.display().to_string();
    let decision = dispatch_project_trust(Some(&handle), None, &cwd_text).await;
    assert_eq!(decision, ProjectTrustDecision::Yes);
    assert!(
        resolve_project_trust(&dir, &agent_dir),
        "the recorded decision must be the trust result"
    );
    // `remember: true` persists to <agent_dir>/trust.json.
    let store = ProjectTrustStore::new(&agent_dir);
    assert_eq!(
        store.get(&dir).map(|entry| entry.decision),
        Some(true),
        "remember:true must persist the decision"
    );
    // The settings load every host uses now reads the project scope.
    let settings = SettingsManager::load(
        &agent_dir,
        &dir.join(".pi"),
        &dir,
        resolve_project_trust(&dir, &agent_dir),
    );
    assert_eq!(
        settings.settings.theme.as_deref(),
        Some("project-theme"),
        "trust-gated project settings must load after the extension decision"
    );

    // An explicit "no" is recorded too and wins over the (yes) trust store,
    // while the operator override still wins over both.
    let handle_no = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle_no,
        &dir,
        "deny.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "no" }));
        }
        "#,
    )
    .await;
    let denied = dispatch_project_trust(Some(&handle_no), None, &cwd_text).await;
    assert_eq!(denied, ProjectTrustDecision::No);
    assert!(!resolve_project_trust(&dir, &agent_dir));
    std::env::set_var("PIR_TRUST_PROJECT", "1");
    assert!(
        resolve_project_trust(&dir, &agent_dir),
        "PIR_TRUST_PROJECT=1 overrides an extension denial"
    );
}

/// §2 #12: a grant is usable by the run gate without persisting anything when
/// the handler omits `remember` (a session-only extension decision).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn project_trust_extension_grant_without_remember_is_session_only() {
    let dir = fixture_dir("project-trust-grant-session");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(dir.join(".pi")).expect("project dir");
    std::fs::write(
        dir.join(".pi/settings.json"),
        r#"{"theme":"session-theme"}"#,
    )
    .expect("project settings");

    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    assert_eq!(
        override_or_extension_decision(&dir),
        None,
        "no extension has decided yet"
    );
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "grant.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "yes" }));
        }
        "#,
    )
    .await;

    let decision = dispatch_project_trust(Some(&handle), None, &dir.display().to_string()).await;
    assert_eq!(decision, ProjectTrustDecision::Yes);
    assert_eq!(
        override_or_extension_decision(&dir),
        Some(true),
        "the extension decision is the run gate's input"
    );
    assert!(resolve_project_trust(&dir, &agent_dir));
    // `remember` omitted => nothing persisted.
    let store = ProjectTrustStore::new(&agent_dir);
    assert_eq!(
        store.get(&dir).map(|entry| entry.decision),
        None,
        "a session-only grant must not write trust.json"
    );
    let settings = SettingsManager::load(&agent_dir, &dir.join(".pi"), &dir, true);
    assert_eq!(
        settings.settings.theme.as_deref(),
        Some("session-theme"),
        "the grant opens the project settings scope"
    );
}

/// §2 #12: an extension denial beats a stored `yes`.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn project_trust_extension_denial_overrides_a_stored_grant() {
    let dir = fixture_dir("project-trust-deny-store");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(dir.join(".pi")).expect("project dir");
    std::fs::write(dir.join(".pi/settings.json"), "{}").expect("project settings");

    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    let store = ProjectTrustStore::new(&agent_dir);
    store.set(&dir, Some(true)).expect("seed the store");
    assert!(resolve_project_trust(&dir, &agent_dir));

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "deny.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "no" }));
        }
        "#,
    )
    .await;
    let decision = dispatch_project_trust(Some(&handle), None, &dir.display().to_string()).await;
    assert_eq!(decision, ProjectTrustDecision::No);
    assert_eq!(
        override_or_extension_decision(&dir),
        Some(false),
        "the extension decision precedes the trust store"
    );
    assert!(
        !resolve_project_trust(&dir, &agent_dir),
        "an extension denial must override a stored yes"
    );
    // A denial without `remember` leaves the stored decision untouched.
    assert_eq!(
        store.get(&dir).map(|entry| entry.decision),
        Some(true),
        "remember:false must not rewrite the store"
    );
}

/// §2 #12: with no extension decision (no extension, or every handler
/// undecided) the store/policy decides exactly as before.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn project_trust_without_an_extension_falls_back_to_store_and_policy() {
    let dir = fixture_dir("project-trust-no-extension");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(dir.join(".pi")).expect("project dir");
    // A trust-requiring resource: without it the project is implicitly
    // trusted and the store is never consulted.
    std::fs::write(dir.join(".pi/settings.json"), "{}").expect("project settings");

    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    assert_eq!(override_or_extension_decision(&dir), None);
    let store = ProjectTrustStore::new(&agent_dir);
    store.set(&dir, Some(true)).expect("store yes");
    assert!(
        resolve_project_trust(&dir, &agent_dir),
        "no extension decision => stored yes"
    );
    store.set(&dir, Some(false)).expect("store no");
    assert!(
        !resolve_project_trust(&dir, &agent_dir),
        "no extension decision => stored no"
    );
    store.set(&dir, None).expect("clear store");
    assert!(
        !resolve_project_trust(&dir, &agent_dir),
        "no decision at all => headless ask resolves to untrusted"
    );
}

/// §2 #12: the run gate's resolved boolean is frozen; a later extension load
/// (the final plan, which can include project extensions) cannot flip it.
/// This is what makes `--no-approve` / a session-only "do not trust" stick.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn project_trust_pinned_run_decision_survives_a_later_dispatch() {
    let dir = fixture_dir("project-trust-pinned");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    pin_decision(&dir, false);
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "late-grant.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "yes", remember: true }));
        }
        "#,
    )
    .await;
    let decision = dispatch_project_trust(Some(&handle), None, &dir.display().to_string()).await;
    assert_eq!(decision, ProjectTrustDecision::Yes);
    assert!(
        !resolve_project_trust(&dir, &agent_dir),
        "the pinned run decision must not be overwritten by the final load"
    );
    assert_eq!(override_or_extension_decision(&dir), Some(false));
    // A pinned run also refuses the later load's `remember: true`: the
    // startup override (`--no-approve`, a session-only "do not trust") must
    // not leak into the persistent store.
    assert_eq!(
        ProjectTrustStore::new(&agent_dir)
            .get(&dir)
            .map(|entry| entry.decision),
        None,
        "a later load must not persist a decision the run did not use"
    );
}

// ---------------------------------------------------------------------------
// Print/json model-environment trust propagation: `run_print` loads the model
// environment AFTER the startup gate resolved (and pinned) project trust, so
// `load_pi_environment` must see that decision instead of re-resolving only the
// trust store.
// ---------------------------------------------------------------------------

/// A project whose `.pi/settings.json` carries a value the model environment
/// projects into `PiEnvironment.settings` (and that only loads when the
/// project scope is trusted).
fn print_environment_project(test_name: &str) -> (PathBuf, PathBuf) {
    let dir = fixture_dir(test_name);
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(dir.join(".pi")).expect("project dir");
    std::fs::write(
        dir.join(".pi/settings.json"),
        r#"{"theme":"project-theme"}"#,
    )
    .expect("project settings");
    (dir, agent_dir)
}

/// The print/json host's model environment must read the project settings when
/// the startup trust pass got an extension grant (the grant need not be
/// remembered: the pin carries the run's decision).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn print_model_environment_reads_project_settings_after_an_extension_grant() {
    let (dir, agent_dir) = print_environment_project("print-env-grant");
    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "grant.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "yes" }));
        }
        "#,
    )
    .await;
    let decision = dispatch_project_trust(Some(&handle), None, &dir.display().to_string()).await;
    assert_eq!(decision, ProjectTrustDecision::Yes);

    let environment = load_pi_environment(Some(&dir)).expect("print model environment");
    assert_eq!(
        environment.settings.theme.as_deref(),
        Some("project-theme"),
        "the print model environment must read project settings after an extension grant"
    );
}

/// An extension denial beats a stored grant for the print/json model
/// environment too: no project settings are read without `remember`.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn print_model_environment_skips_project_settings_after_an_extension_denial() {
    let (dir, agent_dir) = print_environment_project("print-env-deny");
    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    let store = ProjectTrustStore::new(&agent_dir);
    store.set(&dir, Some(true)).expect("seed stored grant");
    let stored = load_pi_environment(Some(&dir)).expect("stored-grant environment");
    assert_eq!(
        stored.settings.theme.as_deref(),
        Some("project-theme"),
        "the store fallback must still open the project scope"
    );

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "deny.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "no" }));
        }
        "#,
    )
    .await;
    let decision = dispatch_project_trust(Some(&handle), None, &dir.display().to_string()).await;
    assert_eq!(decision, ProjectTrustDecision::No);

    let environment = load_pi_environment(Some(&dir)).expect("denied environment");
    assert_eq!(
        environment.settings.theme, None,
        "an extension denial must keep the print model environment off the project settings"
    );
}

/// The startup gate's explicit `--no-approve` pins `false`; the print/json
/// model environment must honor it even when an extension granted trust first
/// and `PIR_TRUST_PROJECT=1` would otherwise re-open it.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn print_model_environment_honors_a_pinned_no_approve_over_an_extension_grant() {
    let (dir, agent_dir) = print_environment_project("print-env-no-approve");
    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "grant.ts",
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "yes" }));
        }
        "#,
    )
    .await;
    let decision = dispatch_project_trust(Some(&handle), None, &dir.display().to_string()).await;
    assert_eq!(decision, ProjectTrustDecision::Yes);
    assert_eq!(override_or_extension_decision(&dir), Some(true));

    // What the startup host does after resolving an explicit `--no-approve`.
    pin_decision(&dir, false);
    // The pinned explicit decision outranks the environment override.
    std::env::set_var("PIR_TRUST_PROJECT", "1");
    assert_eq!(resolved_decision(&dir), Some(false));
    assert!(!resolve_project_trust(&dir, &agent_dir));

    let environment = load_pi_environment(Some(&dir)).expect("pinned environment");
    assert_eq!(
        environment.settings.theme, None,
        "--no-approve must keep the print model environment off the project settings"
    );
}

/// With no extension decision the print/json model environment falls back to
/// exactly the old order: the trust store, then `defaultProjectTrust`.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn print_model_environment_without_an_extension_decision_keeps_store_and_policy_fallback() {
    let (dir, agent_dir) = print_environment_project("print-env-fallback");
    let _lock = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = TrustEnvGuard::new(&agent_dir);

    // No decision + the headless `ask` default => untrusted.
    let untrusted = load_pi_environment(Some(&dir)).expect("default environment");
    assert_eq!(untrusted.settings.theme, None);

    // No extension decision => the stored yes decides.
    let store = ProjectTrustStore::new(&agent_dir);
    store.set(&dir, Some(true)).expect("store yes");
    let stored_yes = load_pi_environment(Some(&dir)).expect("stored-yes environment");
    assert_eq!(stored_yes.settings.theme.as_deref(), Some("project-theme"));

    // ... and the stored no still denies.
    store.set(&dir, Some(false)).expect("store no");
    let stored_no = load_pi_environment(Some(&dir)).expect("stored-no environment");
    assert_eq!(stored_no.settings.theme, None);

    // No store entry => the `defaultProjectTrust` policy decides.
    store.set(&dir, None).expect("clear store");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("agent settings");
    let policy = load_pi_environment(Some(&dir)).expect("policy environment");
    assert_eq!(
        policy.settings.theme.as_deref(),
        Some("project-theme"),
        "defaultProjectTrust:always must still open the project scope"
    );
}

// ---------------------------------------------------------------------------
// Audit §4 #4: sendUserMessage keeps image blocks and expands templates
// ---------------------------------------------------------------------------

fn capture_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake".into(),
        api: "fake".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

struct CapturingProvider {
    contexts: Arc<Mutex<Vec<Context>>>,
    calls: AtomicUsize,
}

impl Provider for CapturingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![capture_model()]
    }
    fn api_key_auth(&self) -> Vec<pir::ai::auth::ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts.lock().unwrap().push(context);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![ContentBlock::Text(pir::ai::types::TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            },
        ));
        stream
    }
}

#[tokio::test(flavor = "current_thread")]
async fn send_user_message_preserves_images_and_expands_prompt_templates() {
    let dir = fixture_dir("send-user-message");
    let skill_file = dir.join("contrib/SKILL.md");
    std::fs::create_dir_all(skill_file.parent().unwrap()).unwrap();
    std::fs::write(
        &skill_file,
        "---\nname: contrib\ndescription: Contributed\n---\nContributed body\n",
    )
    .unwrap();

    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime");
    seed_and_load(
        &handle,
        &dir,
        "send.ts",
        r#"
        export default function (pi) {
            pi.sendUserMessage([
                { type: "text", text: "look at this" },
                { type: "image", data: "aGVsbG8=", mimeType: "image/png" },
            ]);
            pi.sendUserMessage("/skill:contrib hi", { expandPromptTemplates: true });
        }
        "#,
    )
    .await;

    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CapturingProvider {
        contexts: Arc::clone(&contexts),
        calls: AtomicUsize::new(0),
    }));
    let mut session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: capture_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle),
        tool_selection: None,
    });
    session.set_prompt_resources(
        vec![SkillInfo {
            name: "contrib".into(),
            description: "Contributed".into(),
            path: skill_file,
            disable_model_invocation: false,
        }],
        Vec::new(),
    );

    // Idle drain: both messages become pending USER runs (not text-flattened
    // steering entries).
    session.apply_pending_extension_notifications().await;
    let image_run = session.take_pending_custom_run().expect("image run queued");
    assert_eq!(image_run.custom_type, "__pir_user_message");
    session
        .run_custom_turn(image_run, Arc::new(|_| {}))
        .await
        .expect("image run");
    let expanded_run = session
        .take_pending_custom_run()
        .expect("expanded run queued");
    session
        .run_custom_turn(expanded_run, Arc::new(|_| {}))
        .await
        .expect("expanded run");

    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2, "one request per run");
    let first_user_blocks = contexts[0]
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pir::ai::types::Message::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .expect("user message");
    assert!(
        first_user_blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Image(image)
                if image.data == "aGVsbG8=" && image.mime_type == "image/png"
        )),
        "the image block must survive: {first_user_blocks:?}"
    );
    assert!(
        first_user_blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text(text) if text.text.contains("look at this")
        )),
        "the text part must survive: {first_user_blocks:?}"
    );

    let second_text = contexts[1]
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pir::ai::types::Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .expect("user message");
    assert!(
        second_text.contains("<skill name=\"contrib\"") && second_text.contains("Contributed body"),
        "`expandPromptTemplates: true` must expand `/skill:`: {second_text}"
    );
}
