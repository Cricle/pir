//! `fs.watch` real semantics: the host watcher threads (`fs_watch_fns.rs`)
//! queue Node's two event types (`rename` / `change`) and the JS FSWatcher
//! pump delivers them with the Node listener shape
//! `(eventType, filename|null)`. Fixture dirs are created at runtime in the
//! system temp tree (the hygiene gate forbids fixture files in-repo; the fs
//! permission model allows temp reads/writes).
//!
//! Drive pattern: the handler parks on a promise that resolves once BOTH
//! event kinds were observed (or after a bounded JS-side timeout — the test
//! must never hang), so its pending await keeps the JS event-loop pump (the
//! FSWatcher poll timer) running while the Rust thread performs the file
//! operations concurrently.

use std::path::PathBuf;
use std::time::Duration;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

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

#[tokio::test(flavor = "current_thread")]
async fn fs_watch_emits_rename_and_change_events() {
    let dir = std::env::temp_dir().join(format!("pir-fs-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create watch dir");
    let file = dir.join("a.txt");

    let ts = r#"
import fs from "node:fs";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const records: any[] = [];
    let gotRename = false;
    let gotChange = false;
    const settle = (resolve: any) => () => {
      if (gotRename && gotChange) resolve("both");
    };
    const watcher = fs.watch("__PIR_WATCH_DIR__", (eventType: any, filename: any) => {
      records.push({ type: eventType, filename: filename });
      if (eventType === "rename" && String(filename).indexOf("a.txt") !== -1) gotRename = true;
      if (eventType === "change" && String(filename).indexOf("a.txt") !== -1) gotChange = true;
    });
    globalThis.__pirWatchRecs = records;
    globalThis.__pirWatcher = watcher;
    // Timeout protection: a missing event resolves after 15s, never hangs.
    await new Promise((resolve: any) => {
      const check = settle(resolve);
      check();
      setTimeout(() => resolve("timeout"), 15000);
      // Re-check after each event: patch push via records polling.
      const poll = setInterval(() => check(), 50);
      (globalThis as any).__pirWatchStopPoll = () => clearInterval(poll);
    });
    (globalThis as any).__pirWatchStopPoll && (globalThis as any).__pirWatchStopPoll();
  });
};
"#
    .replace(
        "__PIR_WATCH_DIR__",
        &dir.to_string_lossy().replace('\\', "\\\\"),
    );

    let rt = ExtensionRuntime::new(test_config(dir.clone()))
        .await
        .expect("runtime");
    rt.allow_root(dir.clone());
    let module = "pir-ext:watch_fixture.ts";
    rt.seed_module(module, &ts).expect("seed fixture");
    let loaded = rt.seeded_load(module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    // The file operations run while the handler await pumps the JS loop.
    let ops_file = file.clone();
    let ops = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(&ops_file, "one").expect("create file");
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(&ops_file, "one-two").expect("modify file");
    });

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors: {:?}",
        outcome.errors
    );
    ops.join().expect("file ops thread");

    let records: Value = rt
        .eval_json("JSON.stringify(globalThis.__pirWatchRecs || [])")
        .await
        .expect("read records");
    let mut rename_seen = false;
    let mut change_seen = false;
    for record in records.as_array().expect("records array") {
        let kind = record["type"].as_str().unwrap_or("");
        let filename = record["filename"].as_str().unwrap_or("");
        if kind == "rename" && filename.contains("a.txt") {
            rename_seen = true;
        }
        if kind == "change" && filename.contains("a.txt") {
            change_seen = true;
        }
    }
    assert!(rename_seen, "rename event for the created file: {records}");
    assert!(change_seen, "change event for the modified file: {records}");

    // close() stops the pump deterministically.
    let closed = rt
        .eval_json("JSON.stringify({ closed: globalThis.__pirWatcher.close().closed })")
        .await
        .expect("close watcher");
    assert_eq!(closed["closed"], true, "watcher.close() marks closed");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Bounded event backlog: a watcher nobody polls must not grow without limit.
// The fixture drives `__pir_host.fsWatch` directly (no FSWatcher timer), so the
// queue is exactly "no consumer"; overflow drops the OLDEST events, counts
// them in the `dropped` field of every `watchPoll` answer, and polling again
// after the burst resumes delivering new changes.
// ---------------------------------------------------------------------------

/// Events one watcher may queue before dropping the oldest (`MAX_WATCH_EVENTS`
/// in `fs_watch_fns.rs`); mirrored here so the expectation is explicit.
const WATCH_QUEUE_BOUND: usize = 1024;

/// Files created while the poller is silent (files - bound = dropped).
const WATCH_BURST_FILES: usize = 1300;

/// Drain the fixture's watcher through the raw host bridge and summarize the
/// answer (the JS pump is deliberately never started: the queue's consumer is
/// this explicit poll).
///
/// The script is RAW JavaScript (`eval_json` evaluates the text as-is, with no
/// TypeScript transpile — only `seed_module` fixtures go through swc), so it
/// must stay plain JS: a type annotation here is a parse error, which reaches
/// the test as rquickjs's opaque `Exception generated by QuickJS`.
async fn poll_watch(rt: &ExtensionRuntime) -> Value {
    let script = r#"JSON.stringify((() => {
        const raw = JSON.parse(globalThis.__pir_host.fsWatch("watchPoll", JSON.stringify({ id: globalThis.__watchId })));
        const events = raw.value.events || [];
        const names = events.map((event) => String(event.filename));
        return {
            count: events.length,
            dropped: raw.value.dropped || 0,
            unique: new Set(names).size,
            kinds: Array.from(new Set(events.map((event) => event.type))),
        };
    })())"#;
    rt.eval_json(script).await.expect("poll watcher")
}

#[tokio::test(flavor = "current_thread")]
async fn fs_watch_backlog_is_bounded_counted_and_resumes() {
    let dir = std::env::temp_dir().join(format!("pir-fs-watch-bound-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create watch dir");

    let ts = r#"
export default (pi: any) => {
  pi.on("session_start", async () => {
    const host = (globalThis as any).__pir_host;
    const started = JSON.parse(host.fsWatch("watchStart", JSON.stringify({ path: "__PIR_WATCH_DIR__" })));
    (globalThis as any).__watchId = started.value.id;
    // The handler RETURNS immediately: `emit` awaits a handler's returned
    // promise, so parking here would hang the dispatch (the test cannot
    // resolve a fixture promise before the emit returns). No JS consumer
    // ever drains the queue anyway — the FSWatcher poll timer is never
    // started (this fixture uses the raw host bridge, not `fs.watch`), and
    // the test's explicit `watchPoll` is the only drain.
  });
};
"#
    .replace(
        "__PIR_WATCH_DIR__",
        &dir.to_string_lossy().replace('\\', "\\\\"),
    );

    let rt = ExtensionRuntime::new(test_config(dir.clone()))
        .await
        .expect("runtime");
    rt.allow_root(dir.clone());
    let module = "pir-ext:watch_bound_fixture.ts";
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

    // Let the watcher thread take its baseline snapshot before the burst.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for index in 0..WATCH_BURST_FILES {
        let path = dir.join(format!("burst-{index:04}.txt"));
        std::fs::File::create(&path).expect("create burst file");
    }
    // ~6 poll ticks: every created file is observed and reported once.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let first = poll_watch(&rt).await;
    assert_eq!(
        first["count"].as_u64(),
        Some(WATCH_QUEUE_BOUND as u64),
        "the queue holds exactly the bound after a silent burst of {WATCH_BURST_FILES}: {first}"
    );
    assert_eq!(
        first["dropped"].as_u64(),
        Some((WATCH_BURST_FILES - WATCH_QUEUE_BOUND) as u64),
        "every event past the bound is dropped OLDEST-first and counted: {first}"
    );
    assert_eq!(
        first["unique"].as_u64(),
        Some(WATCH_QUEUE_BOUND as u64),
        "the delivered batch has no duplicate filenames: {first}"
    );
    let kinds = first["kinds"].as_array().expect("kinds array");
    assert!(
        kinds.iter().all(|kind| kind.as_str() == Some("rename")),
        "new files report rename only: {first}"
    );

    // The consumer is back: later changes are delivered again, and the
    // cumulative drop count does not move while the queue is drained.
    for index in 0..5 {
        std::fs::File::create(dir.join(format!("after-{index}.txt"))).expect("create file");
    }
    tokio::time::sleep(Duration::from_millis(900)).await;
    let second = poll_watch(&rt).await;
    assert_eq!(
        second["count"].as_u64(),
        Some(5),
        "polling resumes delivery of new changes: {second}"
    );
    assert_eq!(
        second["dropped"].as_u64(),
        first["dropped"].as_u64(),
        "no further drops once the consumer drains: {second}"
    );

    let _ = rt
        .eval_json("JSON.stringify({ stopped: globalThis.__pir_host.fsWatch(\"watchStop\", JSON.stringify({ id: globalThis.__watchId })) })")
        .await;
    let _ = std::fs::remove_dir_all(&dir);
}
