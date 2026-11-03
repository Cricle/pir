//! Lane EVENT3 unit-level handoff regression: a parked strict-validation
//! runtime is reused only when its extension runtime config matches the
//! host's, and a mismatched parked runtime is disposed without emitting a
//! startup-time `session_shutdown`.
//!
//! The end-to-end `--mode json` sequence lives in
//! `lane_event3_json_host.rs`; this file pins the two loader halves that make
//! it work with a real QuickJS extension. Both scenarios share the
//! process-global `VALIDATED_RUNTIME` slot, so they run sequentially in one
//! test (this test binary has no other user of the slot).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use pir::extensions::{ExtensionConfig, ExtensionHandle};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-lane-event3-handoff-{label}-{}-{}",
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

fn config(cwd: &Path, mode: &str) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: mode.to_owned(),
        ..Default::default()
    }
}

fn logging_extension(dir: &Path, log: &Path, factories: &Path) -> PathBuf {
    let log = log.display().to_string();
    let factories = factories.display().to_string();
    let path = dir.join("handoff.ts");
    std::fs::write(
        &path,
        format!(
            r#"import fs from "node:fs";
const LOG = {log:?};
const FACTORIES = {factories:?};
export default function (pi) {{
  fs.appendFileSync(FACTORIES, "factory\n");
  pi.on("session_shutdown", (event) => {{
    fs.appendFileSync(LOG, "session_shutdown:" + (event && event.reason ? event.reason : "") + "\n");
  }});
}}
"#
        ),
    )
    .expect("write extension");
    path
}

fn read_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn strict_runtime(path: &Path, config: ExtensionConfig) -> Option<ExtensionHandle> {
    let paths = [path.to_path_buf()];
    let (runtime, failures) = pir::agent_session::load_extension_runtime_strict(&paths, config)
        .await
        .expect("strict load");
    assert!(
        failures.is_empty(),
        "unexpected load failures: {failures:?}"
    );
    runtime
}
#[tokio::test]
async fn parked_runtime_handoff_reuses_on_match_and_suppresses_on_mismatch() {
    // 1. Matching config: the host consumes the parked runtime and the
    //    factory is not evaluated a second time.
    {
        let temp = TempDir::new("match");
        let log = temp.0.join("events.log");
        let factories = temp.0.join("factories.log");
        let extension = logging_extension(&temp.0, &log, &factories);

        let validation_config = config(&temp.0, "json");
        let Some(runtime) = strict_runtime(&extension, validation_config.clone()).await else {
            // Build without the JS extension runtime.
            return;
        };
        pir::agent_session::stash_validated_extension_runtime(
            std::slice::from_ref(&extension),
            validation_config,
            runtime,
        );

        // The print host's config differs only by `model` (pushed as a
        // session snapshot at handoff), which the slot deliberately ignores.
        let host_config = ExtensionConfig {
            model: Some("mock-model".to_owned()),
            ..config(&temp.0, "json")
        };
        let Some(reused) = pir::agent_session::load_extension_runtime_with_catalog(
            std::slice::from_ref(&extension),
            host_config,
            None,
        )
        .await
        else {
            panic!("matching parked runtime must be consumed");
        };

        assert_eq!(
            read_lines(&factories).len(),
            1,
            "the host handoff must not re-evaluate the extension factory"
        );
        assert!(
            read_lines(&log).is_empty(),
            "the live runtime must not emit session_shutdown before teardown"
        );
        reused.shutdown().await.expect("shutdown");
        assert_eq!(
            read_lines(&log),
            vec!["session_shutdown:quit".to_owned()],
            "the real session teardown emits session_shutdown exactly once"
        );
    }

    // 2. Config mismatch: the parked runtime is dropped without a startup
    //    shutdown; the host's fresh runtime owns the one real shutdown.
    {
        let temp = TempDir::new("mismatch");
        let log = temp.0.join("events.log");
        let factories = temp.0.join("factories.log");
        let extension = logging_extension(&temp.0, &log, &factories);

        let validation_config = config(&temp.0, "print");
        let Some(runtime) = strict_runtime(&extension, validation_config.clone()).await else {
            return;
        };
        // Observe the rejected runtime's actor exit without holding a sender
        // clone (which would keep its mailbox open).
        let rejected_ready = runtime.ready_flag();
        pir::agent_session::stash_validated_extension_runtime(
            std::slice::from_ref(&extension),
            validation_config,
            runtime,
        );

        let Some(fresh) = pir::agent_session::load_extension_runtime_with_catalog(
            std::slice::from_ref(&extension),
            config(&temp.0, "json"),
            None,
        )
        .await
        else {
            panic!("fresh host load");
        };
        assert_eq!(
            read_lines(&factories).len(),
            2,
            "a config mismatch re-evaluates the factory once for the host"
        );
        // The rejected runtime's actor teardown is asynchronous; wait for it
        // to finish before asserting the suppressed tail wrote nothing.
        wait_until(
            || !rejected_ready.load(Ordering::Relaxed),
            "the rejected validation actor to exit",
        )
        .await;
        assert!(
            read_lines(&log).is_empty(),
            "dropping an unconsumed validation runtime must not emit session_shutdown"
        );
        fresh.shutdown().await.expect("shutdown");
        assert_eq!(
            read_lines(&log),
            vec!["session_shutdown:quit".to_owned()],
            "only the host runtime's real teardown emits session_shutdown"
        );
    }
}
