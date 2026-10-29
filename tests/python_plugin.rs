//! Tests for the embedded-RustPython `python` builtin tool
//! (src/plugins/builtin/python.rs) — pir's differentiation from upstream,
//! where Python runs through bash + the user's native python3.
//!
//! Covered, all offline:
//! - pure computation: exec-mode print capture, single-expression repr echo,
//!   no output → "(no output)"
//! - persistent session: definitions from one call visible in the next
//!   (the core value over the stateless bash+python3 path)
//! - failure modes: uncaught exception → `isError` + Python traceback;
//!   empty `code` rejected
//! - sandbox limits: the deadline interrupts a runaway loop (interrupt lands
//!   in well under the 30s default, so the eval-loop signal poll works) and
//!   resets the session; an abort (cancelled `AbortSignal`) does the same;
//!   captured output is capped at 1 MiB
//! - registration: the builtin factory list, `with_config(false)` skipping
//!   registration, and the `disabledBuiltinExtensions` loader filter dropping
//!   `builtin:python`
//! - cwd imports on sys.path (parity with the bash+python3 tool)
//!
//! New file owned by the python-tool task; the `g_*.rs` / `suites/` files
//! belong to another workstream and are untouched.

use std::time::{Duration, Instant};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry};
use pir::plugins::builtin::{PythonPlugin, PythonTool, MAX_OUTPUT_CHARS, PYTHON_TOOL_NAME};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn python_tool() -> PythonTool {
    PythonTool::new()
}

fn tool_ctx() -> ToolContext {
    ToolContext::new(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

async fn run(tool: &PythonTool, args: serde_json::Value) -> ToolResult {
    tool.run(&args, &tool_ctx()).await
}

// ---------------------------------------------------------------------------
// Pure computation / output capture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exec_mode_captures_print() {
    let tool = python_tool();
    let result = run(
        &tool,
        serde_json::json!({ "code": "print('hello from rustpython')\nprint(1 + 2)" }),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("hello from rustpython"),
        "stdout captured, got: {}",
        result.content
    );
    assert!(
        result.content.contains('3'),
        "print output, got: {}",
        result.content
    );
}

#[tokio::test]
async fn single_expression_returns_repr() {
    let tool = python_tool();
    let result = run(&tool, serde_json::json!({ "code": "40 + 2" })).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "42");
}

#[tokio::test]
async fn stdlib_imports_work_offline() {
    let tool = python_tool();
    let result = run(
        &tool,
        serde_json::json!({ "code": "import json\njson.dumps({'a': 1})" }),
    )
    .await;
    // json.dumps is an expression statement in exec mode: no implicit echo.
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "(no output)");
}

#[tokio::test]
async fn clean_run_without_output_says_so() {
    let tool = python_tool();
    let result = run(&tool, serde_json::json!({ "code": "x = 1" })).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "(no output)");
}

#[tokio::test]
async fn empty_code_is_rejected() {
    let tool = python_tool();
    let result = run(&tool, serde_json::json!({ "code": "   \n  " })).await;
    assert!(result.is_error);
    assert!(result.content.contains("code"), "got: {}", result.content);
}

// ---------------------------------------------------------------------------
// Persistent session (the core value over bash+python3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn state_persists_across_calls() {
    let tool = python_tool();
    let setup = run(
        &tool,
        serde_json::json!({ "code": "def add(a, b):\n    return a + b\ncounter = 1" }),
    )
    .await;
    assert!(!setup.is_error, "{}", setup.content);

    let use_it = run(&tool, serde_json::json!({ "code": "add(counter, 41)" })).await;
    assert!(!use_it.is_error, "{}", use_it.content);
    assert_eq!(use_it.content, "42", "globals from call 1 must survive");
}

#[tokio::test]
async fn imports_persist_across_calls() {
    let tool = python_tool();
    let setup = run(&tool, serde_json::json!({ "code": "import math" })).await;
    assert!(!setup.is_error, "{}", setup.content);
    let use_it = run(&tool, serde_json::json!({ "code": "math.gcd(12, 18)" })).await;
    assert!(!use_it.is_error, "{}", use_it.content);
    assert_eq!(use_it.content, "6");
}

// ---------------------------------------------------------------------------
// Failure modes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uncaught_exception_returns_traceback() {
    let tool = python_tool();
    let result = run(
        &tool,
        serde_json::json!({ "code": "print('before')\n1 / 0" }),
    )
    .await;
    assert!(result.is_error, "expected an error result");
    assert!(
        result.content.contains("ZeroDivisionError"),
        "traceback present, got: {}",
        result.content
    );
    assert!(
        result.content.contains("before"),
        "partial stdout kept on the error result, got: {}",
        result.content
    );
}

#[tokio::test]
async fn syntax_error_reports_python_error() {
    let tool = python_tool();
    let result = run(
        &tool,
        serde_json::json!({ "code": "def broken(:\n    pass" }),
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.to_lowercase().contains("error"),
        "got: {}",
        result.content
    );
}

#[tokio::test]
async fn session_survives_an_exception() {
    let tool = python_tool();
    let _ = run(&tool, serde_json::json!({ "code": "boom" })).await;
    let setup = run(&tool, serde_json::json!({ "code": "flag = 'alive'" })).await;
    assert!(!setup.is_error, "{}", setup.content);
    let check = run(&tool, serde_json::json!({ "code": "flag" })).await;
    assert!(!check.is_error, "{}", check.content);
    assert_eq!(
        check.content, "alive",
        "a raised exception must not reset the VM"
    );
}

// ---------------------------------------------------------------------------
// Sandbox limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_interrupts_runaway_loop_and_resets_session() {
    let tool = python_tool();
    let started = Instant::now();
    let result = run(
        &tool,
        serde_json::json!({ "code": "while True: pass", "timeout_ms": 1500 }),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(result.is_error, "expected a timeout error");
    assert!(
        result.content.contains("timed out"),
        "got: {}",
        result.content
    );
    // The interrupt must land through the eval-loop signal poll well below
    // the 30s default deadline — if this stalls, the interrupt wiring broke.
    assert!(
        elapsed < Duration::from_secs(20),
        "runaway loop was not interrupted promptly: {elapsed:?}"
    );

    // The session resets after a timeout: the next call runs on a fresh VM.
    let after = run(&tool, serde_json::json!({ "code": "1 + 1" })).await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(after.content, "2");
}

#[tokio::test]
async fn abort_interrupts_runaway_loop_and_resets_session() {
    // The abort path (ctx.signal cancelled mid-run) must do what the timeout
    // path does: deliver the interrupt to the eval loop and reset the worker.
    // Merely returning the error — the old behavior — left the script running
    // (CPU/side effects) and the session alive, making the error text false.
    let probe_dir =
        std::env::temp_dir().join(format!("pir-python-abort-test-{}", std::process::id()));
    std::fs::create_dir_all(&probe_dir).unwrap_or_else(|error| panic!("temp dir: {error}"));
    let probe = probe_dir.join("probe.txt");
    let probe_path = probe.to_string_lossy().replace('\\', "/");

    let tool = std::sync::Arc::new(python_tool());
    // State defined before the aborted call: the next call must not see it.
    let setup = run(&tool, serde_json::json!({ "code": "flag = 'alive'" })).await;
    assert!(!setup.is_error, "{}", setup.content);

    let signal = pir::ai::types::AbortSignal::new();
    let mut ctx = tool_ctx();
    ctx.signal = Some(signal.clone());

    // A runaway loop with a side effect the test can watch: while it runs,
    // the probe file keeps growing.
    let code = format!(
        "import time\npath = \"{probe_path}\"\nwhile True:\n    open(path, \"a\").write(\"x\")\n    time.sleep(0.01)"
    );
    let started = Instant::now();
    let probe_for_start = probe.clone();
    let run_tool = tool.clone();
    let run_task = tokio::spawn(async move {
        run_tool
            .run(&serde_json::json!({ "code": code }), &ctx)
            .await
    });
    // Arm the abort only once the loop is demonstrably running (the VM start
    // alone can take seconds under load — aborting earlier would fire before
    // the first probe write, which says nothing about interrupt delivery).
    let mut loop_started = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::fs::metadata(&probe_for_start)
            .map(|meta| meta.len())
            .unwrap_or(0)
            > 0
        {
            loop_started = true;
            break;
        }
        if run_task.is_finished() {
            break;
        }
    }
    assert!(loop_started, "the probe loop never started");
    signal.abort();
    let result = run_task.await.expect("run task join");
    let elapsed = started.elapsed();

    assert!(
        result.is_error,
        "expected an abort error, got: {}",
        result.content
    );
    assert!(
        result.content.contains("aborted"),
        "got: {}",
        result.content
    );
    assert!(
        result.content.contains("reset"),
        "abort error names the reset, got: {}",
        result.content
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "abort returned only at the deadline? {elapsed:?}"
    );

    // Interrupt proof: after the abort the runaway loop must be dead, so the
    // probe file stops growing. Without the interrupt the worker keeps
    // appending forever (the bug this guards against). The unoptimized
    // (debug-profile) RustPython unwinds the interrupted loop slowly, so the
    // settle wait POLLS until the size is stable instead of assuming fixed
    // grace windows.
    let read_size = || {
        std::fs::metadata(&probe)
            .map(|meta| meta.len())
            .unwrap_or(0)
    };
    let mut settled = read_size();
    assert!(settled > 0, "the probe loop never ran before the abort");
    // Stability means the size held across FOUR consecutive 500ms reads
    // (>=2s without growth). Shorter windows misread thread-starvation
    // pauses — under a loaded machine the interrupted worker can be
    // descheduled for close to a second before its final unwind completes —
    // for "the loop died", and a single slow iteration (antivirus scanning a
    // freshly written file can stall one open() for hundreds of ms) reads as
    // "still running". The whole loop keeps growing at a steady pace, so
    // sustained stability is only reachable once the interrupt landed and
    // the unwind finished.
    let mut stable = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let current = read_size();
        if current == settled {
            stable = true;
            break;
        }
        settled = current;
    }
    // One more read after the size first appeared stable: the loop must stay
    // dead.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let later = read_size();
    assert!(
        stable && later == settled,
        "the aborted script kept running: no interrupt was delivered \
         (settled={settled}, later={later})"
    );

    // Reset proof: the next call runs on a fresh VM, so the global is gone.
    // (timeout_ms keeps a regression — a new job queued behind the old
    // runaway loop — from hanging the test for the full default deadline.)
    let after = run(
        &tool,
        serde_json::json!({ "code": "'flag' in globals()", "timeout_ms": 3000 }),
    )
    .await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(after.content, "False", "abort must reset the session");

    let _ = std::fs::remove_dir_all(&probe_dir);
}

/// F2 regression: the abort path used to run the bounded interrupt-retry loop
/// synchronously (`std::thread::sleep(10ms)` for up to 5 s) on the calling
/// tokio task, freezing single-threaded hosts. Abort must now return as soon
/// as the first interrupt is queued (the retries continue in the background),
/// while the worker is still interrupted and the session reset as before.
#[tokio::test]
async fn abort_returns_promptly_and_still_resets_the_session() {
    let probe_dir =
        std::env::temp_dir().join(format!("pir-python-abort-prompt-{}", std::process::id()));
    std::fs::create_dir_all(&probe_dir).unwrap_or_else(|error| panic!("temp dir: {error}"));
    let probe = probe_dir.join("started.txt");
    let probe_path = probe.to_string_lossy().replace('\\', "/");

    let tool = std::sync::Arc::new(python_tool());
    // Warm the VM so the abort hits a running job, not interpreter startup.
    let warm = run(&tool, serde_json::json!({ "code": "warm = 'yes'" })).await;
    assert!(!warm.is_error, "{}", warm.content);

    let signal = pir::ai::types::AbortSignal::new();
    let mut ctx = tool_ctx();
    ctx.signal = Some(signal.clone());
    let code = format!(
        "import time\nopen(\"{probe_path}\", \"w\").write(\"started\")\nwhile True:\n    time.sleep(0.01)"
    );
    let tool_for_run = std::sync::Arc::clone(&tool);
    let run_task = tokio::spawn(async move {
        tool_for_run
            .run(&serde_json::json!({ "code": code }), &ctx)
            .await
    });

    let mut started = false;
    for _ in 0..300 {
        if probe.exists() {
            started = true;
            break;
        }
        if run_task.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(started, "the runaway loop never started");

    let abort_started = Instant::now();
    signal.abort();
    let result = run_task.await.expect("run task join");
    let abort_elapsed = abort_started.elapsed();

    assert!(
        result.is_error,
        "expected an abort error, got: {}",
        result.content
    );
    assert!(
        result.content.contains("aborted"),
        "got: {}",
        result.content
    );
    assert!(
        result.content.contains("reset"),
        "abort error names the reset, got: {}",
        result.content
    );
    assert!(
        abort_elapsed < Duration::from_secs(1),
        "abort blocked on the interrupt-retry window: {abort_elapsed:?}"
    );

    // Reset proof: the next call runs on a fresh VM (the interrupt itself is
    // covered by `abort_interrupts_runaway_loop_and_resets_session`).
    let after = run(
        &tool,
        serde_json::json!({ "code": "'warm' in globals()", "timeout_ms": 5000 }),
    )
    .await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(after.content, "False", "abort must reset the session");

    let _ = std::fs::remove_dir_all(&probe_dir);
}

/// A deadline that expires while the call is still QUEUED behind another job
/// must not interrupt the job that owns the VM. Before the fix, the queued
/// call's `deliver_interrupt` wrote into the session's shared user-signal
/// channel, so the RUNNING call consumed the KeyboardInterrupt, and
/// `reset_worker` dropped its session under it. The queued call now only
/// abandons itself (the worker skips it); the running call completes, and the
/// session keeps its globals.
#[tokio::test]
async fn queued_timeout_does_not_interrupt_the_running_call() {
    let probe_dir =
        std::env::temp_dir().join(format!("pir-python-queued-timeout-{}", std::process::id()));
    std::fs::create_dir_all(&probe_dir).unwrap_or_else(|error| panic!("temp dir: {error}"));
    let probe = probe_dir.join("started.txt");
    let probe_path = probe.to_string_lossy().replace('\\', "/");

    let tool = std::sync::Arc::new(python_tool());
    // Warm the VM (the first spawn is slow) and define the state the running
    // call's session must retain.
    let warm = run(&tool, serde_json::json!({ "code": "marker = 'B-ok'" })).await;
    assert!(!warm.is_error, "{}", warm.content);

    // The long call starts first and runs ~2s; it proves it is actually
    // running by writing the probe file before sleeping.
    let long_code = format!(
        "import time\nopen(\"{probe_path}\", \"w\").write(\"running\")\nfor _ in range(80):\n    time.sleep(0.025)"
    );
    let long_tool = std::sync::Arc::clone(&tool);
    let long_task = tokio::spawn(async move {
        long_tool
            .run(&serde_json::json!({ "code": long_code }), &tool_ctx())
            .await
    });
    let mut started = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if probe.exists() {
            started = true;
            break;
        }
        if long_task.is_finished() {
            break;
        }
    }
    assert!(started, "the long call never started running");

    // Its deadline (250ms) expires while it is queued behind the long call.
    let queued = run(
        &tool,
        serde_json::json!({ "code": "1 + 1", "timeout_ms": 250 }),
    )
    .await;
    assert!(
        queued.is_error,
        "expected a timeout error, got: {}",
        queued.content
    );
    assert!(
        queued.content.contains("timed out"),
        "got: {}",
        queued.content
    );
    assert!(
        queued.content.contains("queued"),
        "the error names the queue wait: {}",
        queued.content
    );

    // The running call was never interrupted: it completes normally...
    let long = long_task.await.expect("long task join");
    assert!(
        !long.is_error,
        "the running call was disturbed by the queued timeout: {}",
        long.content
    );

    // ...and its session was not reset (the queued call never owned the VM).
    let after = run(
        &tool,
        serde_json::json!({ "code": "marker", "timeout_ms": 5000 }),
    )
    .await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(
        after.content, "B-ok",
        "the queued timeout must leave the session's globals intact"
    );

    let _ = std::fs::remove_dir_all(&probe_dir);
}

#[tokio::test]
async fn timeout_ms_above_cap_is_clamped() {
    // 10 minutes requested; the tool must clamp to 120s. We don't wait — a
    // tiny program finishing fast proves the clamp didn't reject the call.
    let tool = python_tool();
    let result = run(
        &tool,
        serde_json::json!({ "code": "'clamped'", "timeout_ms": 600_000 }),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "clamped");
}

#[tokio::test]
async fn oversized_output_is_truncated() {
    let tool = python_tool();
    let result = run(
        &tool,
        serde_json::json!({ "code": "print('x' * 1_100_000)" }),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let limit = MAX_OUTPUT_CHARS;
    assert!(
        result.content.len() > limit,
        "output was produced and capped, got {} chars",
        result.content.len()
    );
    assert!(
        result.content.len() < limit + 200,
        "cap + notice, got {} chars",
        result.content.len()
    );
    assert!(
        result.content.contains("truncated"),
        "truncation notice present, got: ...{}",
        &result.content[result.content.len() - 120..]
    );
}

// ---------------------------------------------------------------------------
// Working-directory imports (parity with bash+python3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cwd_is_importable() {
    let dir = std::env::temp_dir().join(format!("pir-python-tool-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("temp dir: {error}"));
    std::fs::write(dir.join("pir_py_tool_mod.py"), "VALUE = 7\n")
        .unwrap_or_else(|error| panic!("temp module: {error}"));

    let tool = python_tool();
    let ctx = ToolContext::new(dir.clone());
    let result = tool
        .run(
            &serde_json::json!({ "code": "import pir_py_tool_mod\npir_py_tool_mod.VALUE" }),
            &ctx,
        )
        .await;
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!result.is_error, "{}", result.content);
    // `import` + attribute access in exec mode: no echo, so the value shows
    // only if the import resolved (a failed import would be an error).
    assert_eq!(result.content, "(no output)");
}

// ---------------------------------------------------------------------------
// Registration / filtering
// ---------------------------------------------------------------------------

#[test]
fn registers_python_tool_with_schema() {
    let plugin = PythonPlugin::new_plugin();
    let mut registrations = PluginRegistrations::default();
    plugin
        .register(&mut RegistrationContext::new(&mut registrations))
        .unwrap_or_else(|error| panic!("register: {error}"));
    assert_eq!(registrations.tools.len(), 1, "exactly the python tool");
    let (def, _backend) = &registrations.tools[0];
    assert_eq!(def.name, PYTHON_TOOL_NAME);
    let description = &def.description;
    assert!(
        description.contains("RustPython"),
        "description names the embedded interpreter: {description}"
    );
    assert!(
        description.contains("C extensions"),
        "description states the C-extension boundary: {description}"
    );
    assert!(
        description.contains("state persists"),
        "description states session persistence: {description}"
    );
    let parameters = def.parameters.clone();
    assert_eq!(parameters["required"][0], "code");
    assert!(parameters["properties"]["timeout_ms"].is_object());
}

#[test]
fn disabled_plugin_registers_nothing() {
    let plugin = PythonPlugin::with_config(false);
    let mut registrations = PluginRegistrations::default();
    plugin
        .register(&mut RegistrationContext::new(&mut registrations))
        .unwrap_or_else(|error| panic!("register: {error}"));
    assert!(
        registrations.tools.is_empty(),
        "pythonTool: false must skip registration"
    );
}

#[test]
fn builtin_factory_list_carries_python() {
    assert!(
        pir::plugins::builtin::builtin_names().contains(&PYTHON_TOOL_NAME),
        "builtin:<python> is registered in the factory list"
    );
    assert!(
        pir::plugins::builtin::instantiate_builtin(PYTHON_TOOL_NAME).is_some(),
        "instantiate_builtin resolves the python plugin"
    );
}

#[test]
fn disabled_builtin_extensions_filter_drops_python() {
    // The loader-level `disabledBuiltinExtensions` filter (shared with
    // subagent/mcp/codemode) must drop `builtin:python` like any other
    // builtin; `--tools python` selection rides the standard registry.
    let paths = vec![
        std::path::PathBuf::from("builtin:python"),
        std::path::PathBuf::from("builtin:mcp"),
    ];
    let mut disabled = std::collections::BTreeSet::new();
    disabled.insert("python".to_owned());
    let filtered = pir::plugins::filter_disabled_builtins(&paths, &disabled);
    assert_eq!(
        filtered,
        vec![std::path::PathBuf::from("builtin:mcp")],
        "builtin:python dropped, others kept"
    );
}

#[test]
fn tool_registry_upserts_python_by_name() {
    // Standard-registry integration: registering the tool object (as
    // `--tools python` selection and plugin-host resolution consume it)
    // works like any other tool.
    let mut registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(PythonTool::new()));
    assert!(registry.names().contains(&PYTHON_TOOL_NAME));
}
