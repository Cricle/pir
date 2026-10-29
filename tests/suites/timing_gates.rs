//! Startup timing gates (upstream `core/timings.ts:6` `PI_TIMING=1` +
//! `main.ts:932-934` `PI_STARTUP_BENCHMARK`): either env gate enables the
//! instrumentation/print, `PI_TIMING` is strict `"1"`, and the benchmark on a
//! non-interactive mode is a hard error with the upstream text.
//!
//! The env matrix mutates process-global env vars, so every in-process test
//! takes the module lock; the spawn tests pass env through `Command` and
//! never touch this process's environment.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Per-spawn scratch agent dir. The spawn tests run in parallel threads of
/// this binary, so a single pid-keyed dir would be shared by concurrent
/// children (and never cleaned up); a sequence suffix keeps each spawn's
/// `PIR_AGENT_DIR` private.
fn spawn_agent_dir() -> std::path::PathBuf {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pir-timing-gates-{}-{sequence}",
        std::process::id()
    ))
}

#[test]
fn timing_gate_is_strict_one() {
    // This test mutates the process-global `PI_TIMING`, so it takes the
    // module lock like every other in-process test in this file.
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Upstream core/timings.ts:6: `process.env.PI_TIMING === "1"`.
    std::env::set_var("PI_TIMING", "1");
    assert!(pir::config::timings::timing_enabled());
    std::env::set_var("PI_TIMING", "true");
    assert!(!pir::config::timings::timing_enabled());
    std::env::set_var("PI_TIMING", "0");
    assert!(!pir::config::timings::timing_enabled());
    std::env::remove_var("PI_TIMING");
    assert!(!pir::config::timings::timing_enabled());
}

#[test]
fn startup_timings_enable_on_either_gate() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Neither gate: disabled.
    std::env::remove_var("PI_TIMING");
    std::env::remove_var("PI_STARTUP_BENCHMARK");
    assert!(!pir::config::timings::StartupTimings::new().enabled());

    // The upstream PI_TIMING gate alone enables the instrumentation.
    std::env::set_var("PI_TIMING", "1");
    assert!(pir::config::timings::StartupTimings::new().enabled());
    std::env::remove_var("PI_TIMING");

    // The pir benchmark flag alone enables it too (isTruthyEnvFlag).
    std::env::set_var("PI_STARTUP_BENCHMARK", "yes");
    assert!(pir::config::timings::startup_benchmark_enabled());
    assert!(pir::config::timings::StartupTimings::new().enabled());
    std::env::remove_var("PI_STARTUP_BENCHMARK");
}

fn spawn_pir(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pir"));
    // No interactive stdin: the mode hosts must never wait on the test
    // runner's console (a wedged child holds the pir.exe build lock).
    cmd.stdin(std::process::Stdio::null());
    // Hermetic agent dir: no real settings/extensions leak into the run. One
    // fresh dir per spawn (three spawn tests share this binary's threads).
    cmd.env("PIR_AGENT_DIR", spawn_agent_dir());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.args(args).output().expect("pir binary should start")
}

/// `PI_STARTUP_BENCHMARK=1` on a non-interactive mode prints the exact
/// upstream error line and exits 1 (upstream main.ts:932-934).
#[test]
fn startup_benchmark_errors_on_non_interactive_modes() {
    let output = spawn_pir(&["-p", "hello"], &[("PI_STARTUP_BENCHMARK", "1")]);
    assert!(!output.status.success(), "print mode must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: PI_STARTUP_BENCHMARK only supports interactive mode"),
        "upstream error line, got: {stderr}"
    );

    // `--mode rpc` routes the default command to RPC: also non-interactive.
    let output = spawn_pir(&["--mode", "rpc"], &[("PI_STARTUP_BENCHMARK", "1")]);
    assert!(!output.status.success(), "rpc mode must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: PI_STARTUP_BENCHMARK only supports interactive mode"),
        "{stderr}"
    );
}

/// Subcommands exit before the mode host upstream (main.ts:596-624), so they
/// never reach the benchmark gate: `pir auth status` succeeds under the flag.
#[test]
fn startup_benchmark_does_not_gate_subcommands() {
    let output = spawn_pir(&["auth", "status"], &[("PI_STARTUP_BENCHMARK", "1")]);
    assert!(
        output.status.success(),
        "subcommands bypass the benchmark gate: {:?} / {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // And without the flag, print mode does not print the gate error.
    let output = spawn_pir(&["-p", "hello"], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("PI_STARTUP_BENCHMARK"),
        "the gate must not fire without the flag: {stderr}"
    );
}
