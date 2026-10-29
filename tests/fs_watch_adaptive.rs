//! CPU7: an idle `fs.watch` backs off instead of re-statting the whole tree
//! five times a second.
//!
//! The host watcher started at a fixed 200 ms cadence: five full sweeps per
//! second per watcher, each statting every entry. It now sweeps on a native
//! `notify` wake-up (when the build carries it) and backs the fallback poll
//! off to 2 s after a quiet second, resetting to 200 ms on any change.
//!
//! The `watchStats` host op (diagnostic, not part of `fs.watch`) is the test
//! seam: a quiet window must not add `N * 15` stat calls (the old cadence);
//! the live interval must read the backed-off value; and a change after the
//! idle period must still be delivered.

#![cfg(feature = "extensions")]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// Entries in the watched tree; one sweep stats each of them.
const TREE_FILES: usize = 64;

fn test_config(cwd: PathBuf) -> ExtensionConfig {
    ExtensionConfig {
        cwd,
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn fixture_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-fs-watch-adaptive-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create watch dir");
    for index in 0..TREE_FILES {
        std::fs::write(dir.join(format!("file-{index:03}.txt")), "unchanged")
            .expect("create fixture file");
    }
    dir
}

fn js_str(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy())
}

/// Read the watcher's diagnostic counters (the raw host bridge, no JS pump).
/// Returns the reply's `value` object: the counters live there, not on the
/// envelope.
async fn watch_stats(rt: &ExtensionRuntime) -> Value {
    let script = r#"JSON.stringify(JSON.parse(globalThis.__pir_host.fsWatch("watchStats", JSON.stringify({ id: globalThis.__watchId }))).value)"#;
    rt.eval_json(script).await.expect("read watch stats")
}

/// Drain the queued events through the raw bridge and return the filenames.
async fn poll_event_names(rt: &ExtensionRuntime) -> Vec<String> {
    let script = r#"JSON.stringify(JSON.parse(globalThis.__pir_host.fsWatch("watchPoll", JSON.stringify({ id: globalThis.__watchId }))).value.events.map((event) => String(event.filename)))"#;
    let names: Value = rt.eval_json(script).await.expect("poll watcher");
    names
        .as_array()
        .expect("event names array")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn idle_watch_backs_off_and_still_delivers_changes() {
    let dir = fixture_dir();
    let ts = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const host = (globalThis as any).__pir_host;
    const started = JSON.parse(host.fsWatch("watchStart", JSON.stringify({ path: __WATCH_DIR__ })));
    (globalThis as any).__watchId = started.value.id;
    // Return immediately: the host thread watches on its own; the test drives
    // `watchStats`/`watchPoll` explicitly.
  });
};
"#
    .replace("__WATCH_DIR__", &js_str(&dir));

    let rt = ExtensionRuntime::new(test_config(dir.clone()))
        .await
        .expect("runtime");
    rt.allow_root(dir.clone());
    let module = "pir-ext:fs_watch_adaptive.ts";
    rt.seed_module(module, &ts).expect("seed fixture");
    assert_eq!(
        rt.seeded_load(module).await.expect("seeded load"),
        1,
        "fixture must load"
    );

    let mut payload = json!({ "reason": "startup" });
    rt.emit("session_start", &mut payload)
        .await
        .expect("emit session_start");

    // Wait for the fast-cadence warm-up + a quiet run to arm the 2 s backoff.
    // CI runners can deliver a burst of native events after the fixture is
    // created; poll the diagnostic instead of assuming a fixed settle time.
    let mut first = watch_stats(&rt).await;
    for _ in 0..40 {
        if first["pollIntervalMs"].as_u64() == Some(2000) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        first = watch_stats(&rt).await;
    }
    assert_eq!(
        first["pollIntervalMs"].as_u64(),
        Some(2000),
        "the watcher must back off after a quiet run: {first}"
    );
    // Idle window: the backed-off cadence sweeps every 2 s, not five times a
    // second.
    tokio::time::sleep(Duration::from_millis(4000)).await;
    let second = watch_stats(&rt).await;

    let sweeps = second["sweeps"].as_u64().unwrap_or(0) - first["sweeps"].as_u64().unwrap_or(0);
    let stat_calls =
        second["statCalls"].as_u64().unwrap_or(0) - first["statCalls"].as_u64().unwrap_or(0);
    // At 2 s there are at most 3 sweeps in 4 s; the old 200 ms cadence had 20.
    assert!(
        sweeps <= 4,
        "an idle watcher must back off (saw {sweeps} sweeps in 4 s): {second}"
    );
    assert!(
        stat_calls < (TREE_FILES as u64) * 10,
        "an idle watcher must not re-stat every entry 20 times (saw {stat_calls} stats for \
         {TREE_FILES} entries): {second}"
    );
    assert_eq!(
        second["pollIntervalMs"].as_u64(),
        Some(2000),
        "the live fallback interval must be the backed-off value: {second}"
    );

    // A change after the idle window is still delivered (native event or the
    // fallback sweep; the 2.6 s budget covers the latter).
    std::fs::write(dir.join("wake.txt"), "x").expect("write wake file");
    tokio::time::sleep(Duration::from_millis(2600)).await;
    let names = poll_event_names(&rt).await;
    assert!(
        names.iter().any(|name| name.contains("wake.txt")),
        "a change after the idle period must be reported: {names:?}"
    );

    let _ = rt
        .eval_json(r#"JSON.stringify(globalThis.__pir_host.fsWatch("watchStop", JSON.stringify({ id: globalThis.__watchId })))"#)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
}
