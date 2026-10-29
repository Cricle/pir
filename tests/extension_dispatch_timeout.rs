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
