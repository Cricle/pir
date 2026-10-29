//! Extension-originated stdio routing (`src/extensions/runtime/ext_stdio.rs`).
//!
//! The interactive TUI owns the terminal surface, so an extension's direct
//! `process.stdout.write` / `process.stderr.write` (the `__pir_host.writeStdout`
//! / `writeStderr` bridge the JS `process` stub calls) must land where the
//! routed `[ext] …` diagnostics land — in the installed `tracing` subscriber,
//! i.e. `<agentDir>/pir.log` in the TUI — instead of in the middle of the
//! frame. A framed-stdout host (`rpc`, `json`) routes extension stdout the
//! same way so raw text can never split a protocol line; `pir js` script and
//! print mode keep writing the real process stdio so a piping consumer still
//! sees the bytes.
//!
//! Two layers are pinned here:
//!   * [`ext_stdio_route`] — the pure destination decision per host mode;
//!   * a runtime-level run of extensions writing to both streams, with the
//!     TUI's own subscriber shape installed (a `tracing_subscriber::fmt`
//!     subscriber writing into a log FILE, exactly what `main_cli/run_mode.rs`
//!     installs for the TUI), asserting the routed bytes reached the file,
//!     that an oversized write was capped, and that a headless runtime in the
//!     same process did NOT reach it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pir::extensions::{
    ext_stdio_route, ExtStdioRoute, ExtensionConfig, ExtensionRuntime, MAX_ROUTED_WRITE_BYTES,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// The destination decision
// ---------------------------------------------------------------------------

#[test]
fn ext_stdio_route_decision_per_host_mode() {
    // The interactive TUI host is the only host pair that owns the terminal
    // surface (mode `interactive`/`tui` + the UI declaration).
    assert_eq!(ext_stdio_route("interactive", true), ExtStdioRoute::Routed);
    assert_eq!(ext_stdio_route("tui", true), ExtStdioRoute::Routed);

    // Every headless real-stdio host keeps the historical behavior: `pir js`
    // scripts, print, embedded sessions.
    for mode in ["print", "script", "headless", "Interactive", ""] {
        assert_eq!(
            ext_stdio_route(mode, false),
            ExtStdioRoute::RealStdio,
            "mode {mode:?} without a UI must stay on real stdio"
        );
    }
    // RPC declares a UI but speaks a framed protocol over stdout: extension
    // stdout is protocol-safe (routed to the diagnostic channel) while stderr
    // stays byte-exact.
    assert_eq!(ext_stdio_route("rpc", true), ExtStdioRoute::ProtocolSafe);
    // A headless harness that labels itself `interactive` (mode only, no UI)
    // does not own a frame either.
    assert_eq!(
        ext_stdio_route("interactive", false),
        ExtStdioRoute::RealStdio
    );
}

#[test]
fn protocol_hosts_never_write_extension_stdout_to_the_frame_channel() {
    // The JSON print protocol is the same framed-stdout contract as RPC.
    assert_eq!(ext_stdio_route("json", false), ExtStdioRoute::ProtocolSafe);
    assert_eq!(ext_stdio_route("json", true), ExtStdioRoute::ProtocolSafe);
}

// ---------------------------------------------------------------------------
// The write path, against a real log file
// ---------------------------------------------------------------------------

/// The TUI's own destination shape: a log file under the temp dir, standing in
/// for `<agentDir>/pir.log` (`main_cli/run_mode.rs`).
fn capture_log_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("pir-ext-stdio-routing-{}.log", std::process::id()))
    })
}

/// Install the capture subscriber once per test process. `set_global_default`
/// (not `.init()`) is used deliberately: a second installer fails with `Err`
/// instead of panicking the whole test binary. The `tracing` global is what
/// [`pir::logging`] consults (`dispatcher::has_been_set`), so this is the same
/// switch the TUI flips at startup.
fn install_capture_subscriber() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let path = capture_log_path();
        let _ = std::fs::remove_file(path);
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return false;
        };
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::sync::Arc::new(file))
            .finish();
        tracing::subscriber::set_global_default(subscriber).is_ok()
    })
}

fn fixture_dir(test_name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-ext-stdio-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn config_at(cwd: &Path, mode: &str, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: mode.to_string(),
        has_ui,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Load one fixture extension and dispatch `session_start` so its handler runs
/// the stdio writes. Same harness shape as `tests/console_shim.rs`.
async fn run_stdio_fixture(test_name: &str, source: &str, mode: &str, has_ui: bool) {
    let dir = fixture_dir(test_name);
    let runtime = ExtensionRuntime::new(config_at(&dir, mode, has_ui))
        .await
        .expect("runtime construction");
    runtime.allow_root(dir);
    let module = format!("pir-ext:{test_name}.ts");
    runtime.seed_module(&module, source).expect("seed fixture");
    let loaded = runtime.seeded_load(&module).await.expect("seeded load");
    assert_eq!(loaded, 1, "fixture {test_name} must load");
    let mut payload = json!({ "reason": "startup" });
    let outcome = runtime
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {test_name}: {:?}",
        outcome.errors
    );
}

const ROUTED_STDIO_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", () => {
    process.stdout.write("ext-stdio-routed-stdout\n");
    process.stderr.write("ext-stdio-routed-stderr\n");
  });
};
"#;

const HEADLESS_STDIO_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", () => {
    process.stdout.write("ext-stdio-headless-stdout\n");
    process.stderr.write("ext-stdio-headless-stderr\n");
  });
};
"#;

const BIG_WRITE_PREFIX: &str = "ext-stdio-big-";
const BIG_WRITE_FILL: usize = 8192;
const BIG_WRITE_SUFFIX: &str = "-tail";

#[tokio::test(flavor = "current_thread")]
async fn interactive_session_routes_extension_stdio_into_the_log() {
    assert!(
        install_capture_subscriber(),
        "a global tracing subscriber must be installable in this test process"
    );
    let log_path = capture_log_path().clone();

    // -- interactive TUI host: both streams reach the log -------------------
    run_stdio_fixture("routed", ROUTED_STDIO_TS, "interactive", true).await;
    let log = std::fs::read_to_string(&log_path).expect("log file");
    assert!(
        log.contains("[ext] stdout: ext-stdio-routed-stdout"),
        "routed stdout write missing from the log; log was: {log}"
    );
    assert!(
        log.contains("[ext] stderr: ext-stdio-routed-stderr"),
        "routed stderr write missing from the log; log was: {log}"
    );

    // -- per-write cap: one oversized write is bounded, not dropped ---------
    let big_source = format!(
        r#"
export default (pi: any) => {{
  pi.on("session_start", () => {{
    process.stderr.write("{BIG_WRITE_PREFIX}" + "x".repeat({BIG_WRITE_FILL}) + "{BIG_WRITE_SUFFIX}\n");
  }});
}};
"#
    );
    run_stdio_fixture("routed-big", &big_source, "interactive", true).await;
    let log = std::fs::read_to_string(&log_path).expect("log file");
    let dropped =
        BIG_WRITE_PREFIX.len() + BIG_WRITE_FILL + BIG_WRITE_SUFFIX.len() - MAX_ROUTED_WRITE_BYTES;
    assert!(
        log.contains(&format!(
            "[ext] stderr: {BIG_WRITE_PREFIX}{}",
            "x".repeat(16)
        )),
        "capped prefix missing from the log; log was: {log}"
    );
    assert!(
        log.contains(&format!("[{dropped} more bytes truncated]")),
        "truncation marker missing from the log; log was: {log}"
    );
    assert!(
        !log.contains(BIG_WRITE_SUFFIX),
        "the bytes past the cap leaked into the log; log was: {log}"
    );

    // -- headless host (print): real stdio, nothing in the TUI log ----------
    // The payload's absence from the log IS the assertion; the bytes go to the
    // test process's real stdout/stderr instead (visible without `--nocapture`
    // suppression), which is the contract for a piping consumer.
    let before = log.len();
    run_stdio_fixture("headless", HEADLESS_STDIO_TS, "print", false).await;
    let after = std::fs::read_to_string(&log_path).expect("log file");
    let added = after.get(before..).unwrap_or_default();
    assert!(
        !added.contains("ext-stdio-headless-stdout"),
        "print-mode stdout must not be routed into the TUI log: {added}"
    );
    assert!(
        !added.contains("ext-stdio-headless-stderr"),
        "print-mode stderr must not be routed into the TUI log: {added}"
    );
}
