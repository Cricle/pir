//! Public QuickJS diagnostic compatibility through `pir js` (no Node required).
//!
//! With `autotests = false`, include this file in a harness or register it as
//! a standalone `[[test]]` with `required-features = ["cli", "extensions"]`.
//! Each child gets a clean agent dir and an explicit per-script heap cap.
//! The default exercises embedded AOT shims; set `PIR_TEST_SHIM_SOURCE=1`
//! to exercise the worktree sources instead.

#![cfg(all(feature = "cli", feature = "extensions"))]

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const CHECK: &str = r#"
function check(condition, message) {
  if (!condition) throw new Error(message);
}
"#;

fn run_diagnostic(name: &str, cap_mib: &str, script: &str) {
    struct Workspace(PathBuf);
    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let workspace = Workspace(std::env::temp_dir().join(format!(
        "pir-v8-{}-{name}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::create_dir_all(workspace.0.join("agent")).expect("create diagnostic workspace");

    let source = std::env::var("PIR_TEST_SHIM_SOURCE").as_deref() == Ok("1");
    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .args(["js", "-e", &format!("{CHECK}\n{script}")])
        .current_dir(&workspace.0)
        .env("PIR_AGENT_DIR", workspace.0.join("agent"))
        .env("PIR_JS_MEMORY_MB", cap_mib)
        .env("PIR_EXT_SHIM_SRC", if source { "1" } else { "0" })
        .env("PIR_EXT_BYTECODE", if source { "0" } else { "1" })
        .env("PIR_EXT_BYTECODE_CACHE", "0")
        .env_remove("PIR_EXT_SKIP_NODE_BUILTINS")
        .env_remove("PIR_EXT_SKIP_PRELUDE")
        .env_remove("PIR_QJS_ALLOC")
        .output()
        .expect("run pir js");
    assert!(
        output.status.success(),
        "{name}: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn v8_heap_statistics_report_live_quickjs_bytes_and_capped_headroom() {
    run_diagnostic(
        "heap",
        "64",
        r#"
const v8 = require('node:v8');
const before = v8.getHeapStatistics();
const unsupported = [
  'total_heap_size_executable', 'total_physical_size', 'peak_malloced_memory',
  'does_zap_garbage', 'number_of_native_contexts', 'number_of_detached_contexts',
  'total_global_handles_size', 'used_global_handles_size', 'external_memory',
];
const measured = [
  'total_heap_size', 'total_available_size', 'used_heap_size', 'heap_size_limit',
  'malloced_memory',
];
check(Object.keys(before).length === unsupported.length + measured.length, 'heap statistics shape');
for (const key of unsupported) check(before[key] === 0, key + ' must be a zero placeholder');
for (const key of measured) check(Number.isFinite(before[key]) && before[key] > 0, key);
check(before.heap_size_limit === 64 * 1024 * 1024, 'configured QuickJS cap in bytes');
check(before.total_heap_size === before.malloced_memory, 'allocator bytes');
check(before.total_available_size === before.heap_size_limit - before.malloced_memory, 'headroom');
const bytes = 2 * 1024 * 1024;
const retained = new ArrayBuffer(bytes);
const after = v8.getHeapStatistics();
check(retained.byteLength === bytes && after !== before, 'fresh sample with live allocation');
check(after.used_heap_size - before.used_heap_size >= bytes / 2, 'used bytes must grow');
check(after.malloced_memory - before.malloced_memory >= bytes / 2, 'allocator bytes must grow');
check(after.total_available_size < before.total_available_size, 'headroom must fall');
"#,
    );
}

#[test]
fn v8_heap_statistics_uncapped_and_gc_profiler_reports_unsupported() {
    run_diagnostic(
        "uncapped",
        "0",
        r#"
const v8 = require('v8');
const stats = v8.getHeapStatistics();
check(stats.heap_size_limit === 0 && stats.total_available_size === 0, 'no invented cap or headroom');
check(stats.used_heap_size > 0, 'uncapped runtime still reports usage');
const profiler = new v8.GCProfiler();
for (const method of ['start', 'stop']) {
  let code;
  try { profiler[method](); } catch (err) { code = err.code; }
  check(code === 'ERR_NOT_SUPPORTED', 'GCProfiler.' + method + ' must report unsupported');
}
"#,
    );
}

#[test]
fn v8_expose_gc_enables_collection_in_new_vm_contexts_only() {
    run_diagnostic(
        "expose_gc",
        "64",
        r#"
const v8 = require('node:v8');
const vm = require('node:vm');
const earlier = vm.createContext();
check(typeof globalThis.gc === 'undefined', 'main global default');
check(vm.runInNewContext('typeof gc') === 'undefined', 'VM default');
const tagBefore = v8.cachedDataVersionTag();
check(Number.isInteger(tagBefore) && tagBefore > 0, 'non-zero cache tag');
check(v8.cachedDataVersionTag() === tagBefore, 'cache tag stable until flags change');
v8.setFlagsFromString('--no-expose-gc --expose-gc-extra');
check(v8.cachedDataVersionTag() !== tagBefore, 'cache tag follows the flag set');
check(vm.runInNewContext('typeof gc') === 'undefined', 'other flags do not enable gc');
v8.setFlagsFromString('--expose-gc');
check(!Object.prototype.hasOwnProperty.call(globalThis, 'gc'), 'main global unchanged');
check(vm.runInContext('typeof gc', earlier) === 'undefined', 'existing context unchanged');
check(vm.runInNewContext('gc', { gc: 123 }) === 123, 'sandbox property takes precedence');
const gc = vm.runInNewContext('gc');
check(typeof gc === 'function' && gc === vm.runInNewContext('globalThis.gc'), 'VM gc callable');
const bytes = 2 * 1024 * 1024;
let cycle = { payload: new ArrayBuffer(bytes) };
cycle.self = cycle;
const live = v8.getHeapStatistics().used_heap_size;
check(cycle.payload.byteLength === bytes, 'keep the cycle live for the sample');
cycle = null;
check(gc() === undefined, 'synchronous collector return');
check(live - v8.getHeapStatistics().used_heap_size >= bytes / 2, 'collector must reclaim cycle');
"#,
    );
}
