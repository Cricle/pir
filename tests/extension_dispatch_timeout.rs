//! A blocking extension event handler is bounded by the configured call
//! timeout.
//!
//! The actor-side QuickJS deadline cannot fire while the actor thread is in a
//! long synchronous JS turn (the tokio timer is never polled). The host-side
//! deadline in `ExtensionHandle::emit`/`emit_read_only` enforces the same
//! configured budget from the caller's runtime, so a stuck extension cannot
//! hold the first prompt hostage indefinitely.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pir::extensions::{ExtensionConfig, ExtensionHandle, ExtensionLimits};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ext-timeout-{label}-{}-{}",
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

/// A synchronous host fn (`spawnSync` with no timeout) wedges the actor
/// thread, so it stops draining the 64-slot mailbox. Fill the mailbox and
/// stack more senders behind it: every `emit_with_timeout` must still return
/// its own timeout instead of blocking in `tx.send`, and no timed-out sender
/// may leak a `pending_commands` slot.
const BLOCKING_HOST_FN_EXTENSION: &str = r#"
import { spawnSync } from "node:child_process";

export default function (pi) {
    let blocked = false;
    pi.on("session_start", () => {
        if (blocked) return;
        blocked = true;
        const command = process.platform === "win32"
            ? "ping -n 4 127.0.0.1 > NUL"
            : "sleep 3";
        // No `timeout` option: the host fn blocks the actor un-interruptibly.
        spawnSync(command, [], { shell: true });
    });
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_mailbox_with_wedged_actor_times_out_and_clears_pending() {
    let temp = TempDir::new("mailbox-full");
    let handle = start_and_load(&temp, BLOCKING_HOST_FN_EXTENSION).await;

    // First dispatch wedges the actor inside the synchronous host fn.
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut payload = serde_json::json!({ "type": "session_start", "reason": "block" });
            handle
                .emit_with_timeout("session_start", &mut payload, Duration::from_millis(150))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        handle.pending_commands_for_tests() >= 1,
        "the wedging dispatch must already be in flight"
    );

    // 80 concurrent emits: one in the actor, 64 in the mailbox, the rest
    // waiting for a slot. None may block past its own 150 ms deadline.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..80 {
        let handle = handle.clone();
        tasks.spawn(async move {
            let mut payload = serde_json::json!({ "type": "session_start", "reason": "fill" });
            handle
                .emit_with_timeout("session_start", &mut payload, Duration::from_millis(150))
                .await
        });
    }
    let joined = tokio::time::timeout(Duration::from_secs(10), async {
        let mut results = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            results.push(joined.expect("emit task must not panic"));
        }
        results
    })
    .await
    .expect("a full mailbox must not block an emit past its deadline");
    assert_eq!(joined.len(), 80);
    assert!(
        joined.iter().all(|result| result.is_err()),
        "every emit behind the wedged actor must report a timeout"
    );

    let first_result = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the wedging dispatch itself must be bounded")
        .expect("join the first dispatch");
    assert!(
        first_result.is_err(),
        "the wedging dispatch must time out too"
    );

    // The actor drains the backlog after the synchronous host fn returns;
    // every accepted command releases its pending slot. The senders that
    // never got a slot never allocated one.
    let deadline = Instant::now() + Duration::from_secs(20);
    while handle.pending_commands_for_tests() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        handle.pending_commands_for_tests(),
        0,
        "timed-out mailbox handoffs must not leak pending_commands slots"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_event_handler_returns_at_the_call_timeout() {
    let temp = TempDir::new("emit");
    let entry = temp.0.join("blocking.js");
    std::fs::write(
        &entry,
        r#"
export default function (pi) {
    pi.on("session_start", () => {
        // A tight synchronous turn: no await, so the actor's own tokio timer
        // never gets polled while this runs.
        const end = Date.now() + 10_000;
        while (Date.now() < end) {}
    });
}
"#,
    )
    .expect("write extension fixture");

    let limits = ExtensionLimits {
        call_timeout: Duration::from_millis(200),
        ..ExtensionLimits::default()
    };
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(limits),
    })
    .expect("start the extension actor");
    handle
        .load(&[entry])
        .await
        .expect("load the blocking extension");

    let started = Instant::now();
    let mut payload = serde_json::json!({ "type": "session_start", "reason": "test" });
    let result = handle.emit("session_start", &mut payload).await;
    let elapsed = started.elapsed();
    assert!(
        result.is_err(),
        "the blocked dispatch must report a timeout, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the dispatch must be capped near the 200 ms call timeout, took {elapsed:?}"
    );
}

/// Registers one busy-wait tool, slash command and shortcut: every entry
/// point spins in a synchronous JS turn that the actor-side deadline cannot
/// interrupt (the tokio timer is never polled), so only the caller-side
/// deadline can end the wait.
const BUSY_EXTENSION: &str = r#"
export default function (pi) {
    const spin = () => {
        const end = Date.now() + 10_000;
        while (Date.now() < end) {}
    };
    pi.registerTool({
        name: "busy",
        parameters: { type: "object", properties: {} },
        execute: async () => {
            spin();
            return { content: [{ type: "text", text: "late" }] };
        },
    });
    pi.registerCommand("busy", { handler: async () => { spin(); } });
    pi.registerShortcut("ctrl+shift+b", {
        description: "busy",
        handler: () => { spin(); },
    });
}
"#;

fn test_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(ExtensionLimits {
            call_timeout: Duration::from_millis(200),
            ..ExtensionLimits::default()
        }),
    }
}

async fn start_and_load(temp: &TempDir, source: &str) -> ExtensionHandle {
    let entry = temp.0.join("extension.js");
    std::fs::write(&entry, source).expect("write extension fixture");
    let handle = ExtensionHandle::start(test_config(&temp.0)).expect("start the extension actor");
    handle.load(&[entry]).await.expect("load the extension");
    handle
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_tool_call_returns_at_the_call_timeout() {
    let temp = TempDir::new("tool");
    let handle = start_and_load(&temp, BUSY_EXTENSION).await;

    let started = Instant::now();
    let result = handle
        .call_tool("busy", "call-1", &serde_json::json!({}))
        .await;
    let elapsed = started.elapsed();
    let error = result.expect_err("a blocked tool call must report a timeout");
    assert!(
        error.to_string().contains("timed out"),
        "unexpected error: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the tool call must be capped near the 200 ms call timeout, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_slash_command_returns_at_the_call_timeout() {
    let temp = TempDir::new("command");
    let handle = start_and_load(&temp, BUSY_EXTENSION).await;

    let started = Instant::now();
    let result = handle.dispatch_command("busy", "").await;
    let elapsed = started.elapsed();
    let error = result.expect_err("a blocked slash command must report a timeout");
    assert!(
        error.to_string().contains("timed out"),
        "unexpected error: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the command must be capped near the 200 ms call timeout, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_shortcut_returns_at_the_call_timeout() {
    let temp = TempDir::new("shortcut");
    let handle = start_and_load(&temp, BUSY_EXTENSION).await;

    let started = Instant::now();
    let result = handle.invoke_shortcut("ctrl+shift+b").await;
    let elapsed = started.elapsed();
    let error = result.expect_err("a blocked shortcut must report a timeout");
    assert!(
        error.to_string().contains("timed out"),
        "unexpected error: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the shortcut must be capped near the 200 ms call timeout, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn responsive_commands_still_reply_within_the_deadline() {
    let temp = TempDir::new("responsive");
    let handle = start_and_load(
        &temp,
        r#"
export default function (pi) {
    pi.registerTool({
        name: "echo",
        parameters: { type: "object", properties: {} },
        execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
    });
    pi.registerCommand("hello", { handler: async () => {} });
    pi.registerShortcut("ctrl+shift+h", { handler: () => {} });
}
"#,
    )
    .await;

    let result = handle
        .call_tool("echo", "call-1", &serde_json::json!({}))
        .await
        .expect("a responsive tool replies");
    assert_eq!(result.content[0].text, "ok");
    let outcome = handle
        .dispatch_command("hello", "")
        .await
        .expect("a responsive command replies");
    assert!(outcome.ok, "{:?}", outcome.error);
    let outcome = handle
        .invoke_shortcut("ctrl+shift+h")
        .await
        .expect("a responsive shortcut replies");
    assert!(outcome.ok, "{:?}", outcome.error);
}
