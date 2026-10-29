//! Background-job preemption and the `before_agent_start` budget.
//!
//! The measured first-prompt stall was a package handler awaiting an
//! advertisement promise started at `session_start`; the promise's
//! continuation ran a long SYNCHRONOUS scan on the single actor thread while
//! no command was in flight, so no command-owned deadline existed. These
//! tests pin the generic behavior that fixes it:
//!
//! 1. a queued host event preempts a runaway background continuation (the
//!    actor yields after the configured grace instead of waiting for the
//!    scan);
//! 2. a fast handler is unaffected;
//! 3. a `before_agent_start` handler stuck behind a slow host future is
//!    aborted at `beforeAgentStartTimeoutMs`, the caller gets a timeout, its
//!    late side effects are dropped, and the actor is usable immediately
//!    afterwards;
//! 4. the abandoned dispatch's late completion is not applied to the next
//!    event.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;

use pir::extensions::{
    ExtensionConfig, ExtensionHandle, ExtensionLimits, Notification, DEFAULT_CALL_TIMEOUT,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ext-preempt-{label}-{}-{}",
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

fn config(dir: &TempDir, limits: ExtensionLimits) -> ExtensionConfig {
    ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(limits),
    }
}

async fn load_extension(dir: &TempDir, source: &str, limits: ExtensionLimits) -> ExtensionHandle {
    let entry = dir.0.join("extension.js");
    std::fs::write(&entry, source).expect("write extension fixture");
    let handle = ExtensionHandle::start(config(dir, limits)).expect("start the extension actor");
    handle
        .load(&[entry])
        .await
        .expect("load the extension fixture");
    handle
}

#[tokio::test(flavor = "current_thread")]
async fn queued_event_preempts_a_runaway_background_job() {
    let dir = TempDir::new("background");
    // The callback runs as a promise continuation with no command in flight
    // and no awaits: only the QuickJS interrupt can stop it.
    let handle = load_extension(
        &dir,
        r#"
export default function (pi) {
    pi.on("session_start", () => {
        setTimeout(() => {
            globalThis.__pirBackgroundStarted = true;
            const end = Date.now() + 60_000;
            while (Date.now() < end) { /* runaway synchronous scan */ }
            globalThis.__pirBackgroundFinished = true;
        }, 0);
    });
}
"#,
        ExtensionLimits::default(),
    )
    .await;

    let mut start = json!({ "type": "session_start", "reason": "test" });
    handle
        .emit("session_start", &mut start)
        .await
        .expect("session_start dispatch");

    // Let the timer fire and the background continuation start spinning.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    let mut input = json!({ "type": "input", "text": "hello" });
    handle
        .emit("input", &mut input)
        .await
        .expect("the queued event must preempt the background job");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "the actor must yield to a queued event quickly, took {elapsed:?}"
    );

    // The background spin started and was aborted, not completed.
    let started_background = handle
        .eval_json(r#"JSON.stringify(globalThis.__pirBackgroundStarted === true)"#)
        .await
        .expect("eval probe after recovery");
    assert_eq!(
        started_background,
        json!(true),
        "the fixture's background continuation must actually have run"
    );
    let finished = handle
        .eval_json(r#"JSON.stringify(globalThis.__pirBackgroundFinished === true)"#)
        .await
        .expect("eval probe after recovery");
    assert_eq!(finished, json!(false), "the runaway job was interrupted");

    // The actor is immediately usable for another event.
    let started = Instant::now();
    let mut next = json!({ "type": "input", "text": "again" });
    handle
        .emit("input", &mut next)
        .await
        .expect("second event after preemption");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the actor must stay usable after a preemption"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fast_before_agent_start_handler_is_unaffected() {
    let dir = TempDir::new("fast");
    let handle = load_extension(
        &dir,
        r#"
export default function (pi) {
    pi.on("before_agent_start", () => ({ systemPrompt: "fast-prompt" }));
}
"#,
        ExtensionLimits::default(),
    )
    .await;

    let started = Instant::now();
    let mut payload = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "hi",
        "systemPrompt": "base",
        "systemPromptOptions": {},
    });
    let outcome = handle
        .emit_before_agent_start(&mut payload)
        .await
        .expect("fast handler");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a fast handler must not wait for the budget"
    );
    assert_eq!(outcome.result["systemPrompt"], json!("fast-prompt"));
}

#[tokio::test(flavor = "current_thread")]
async fn slow_before_agent_start_is_aborted_at_the_configured_budget() {
    let dir = TempDir::new("slow");
    let limits = ExtensionLimits {
        call_timeout: Duration::from_secs(30),
        before_agent_start_timeout: Duration::from_millis(250),
        ..ExtensionLimits::default()
    };
    // The handler awaits a host future (the timer) and then queues a side
    // effect. The actor-side budget must abort it so the actor is free when
    // the caller stops waiting, and the late side effect must be dropped.
    let handle = load_extension(
        &dir,
        r#"
export default function (pi) {
    let calls = 0;
    pi.on("before_agent_start", async () => {
        calls += 1;
        if (calls > 1) return { systemPrompt: "second-prompt" };
        await new Promise((resolve) => setTimeout(resolve, 1500));
        pi.sendMessage({ customType: "late", content: "late", display: true });
        return { systemPrompt: "late-prompt" };
    });
}
"#,
        limits,
    )
    .await;

    let started = Instant::now();
    let mut payload = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "hi",
        "systemPrompt": "base",
        "systemPromptOptions": {},
    });
    let error = handle
        .emit_before_agent_start(&mut payload)
        .await
        .expect_err("the stuck handler must time out");
    let elapsed = started.elapsed();
    assert!(
        error.to_string().contains("timed out"),
        "timeout error text: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the 250 ms budget must apply, took {elapsed:?}"
    );

    // The actor is usable for the next event immediately (the handler was
    // aborted actor-side, not merely abandoned caller-side).
    let started = Instant::now();
    let mut next = json!({ "type": "input", "text": "next" });
    handle
        .emit("input", &mut next)
        .await
        .expect("actor usable after the timeout");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "actor must be free right after the timed-out dispatch"
    );

    // Past the handler's original 1500 ms wait: its late completion must not
    // queue anything for a later run.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let notifications = handle.drain_notifications();
    assert!(
        !notifications
            .iter()
            .any(|notification| matches!(notification, Notification::SendMessage { .. })),
        "an abandoned dispatch's late side effects must be dropped: {notifications:?}"
    );

    // And the run's payload/result cannot be applied retroactively: a later
    // dispatch still sees its own result.
    let mut again = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "second",
        "systemPrompt": "base",
        "systemPromptOptions": {},
    });
    let outcome = handle
        .emit_before_agent_start(&mut again)
        .await
        .expect("second dispatch (the stuck handler is gone)");
    assert_eq!(
        outcome.result["systemPrompt"],
        json!("second-prompt"),
        "no late system prompt may be applied to the later run: {outcome:?}"
    );
    // The configured per-call budget is untouched by the shorter lifecycle
    // budget.
    assert_eq!(
        ExtensionLimits::default().call_timeout,
        DEFAULT_CALL_TIMEOUT,
        "the default per-call budget is unchanged"
    );
}
