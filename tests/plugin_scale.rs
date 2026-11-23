//! Plugin-scale benchmark: how the extension host behaves when ONE process
//! hosts many synthesized pi extension packages.
//!
//! The goal this measures is the "hundreds to thousands of heavy plugins with
//! essentially no performance impact" target. Every fixture package is a real
//! on-disk directory (`package.json` manifest + `index.ts` entry + a per-plugin
//! local module) that imports `node:fs` / `node:crypto` / `node:path` and a
//! module shared by every plugin, so module records, per-plugin registries and
//! transpiled-source retention are exercised through the same public
//! `ExtensionRuntime::load` path production uses.
//!
//! For N in {1, 10, 100} the test records:
//!   - load wall time (discovery + transpile + QuickJS compile/execute),
//!   - process current commit / peak working set / current working set / private bytes
//!     (`GetProcessMemoryInfo`, same helper style as
//!     `tests/native_workload_parity.rs`),
//!   - the runtime's QuickJS heap counters (`__pir_host.v8HeapStatistics`,
//!     the same `JS_ComputeMemoryUsage` numbers `node:v8` exposes),
//!   - then calls every plugin's registered tool once and asserts all N
//!     respond.
//!
//! It is `#[ignore]` and release-only: run explicitly with
//!   cargo test --release --test plugin_scale -- --ignored --nocapture
//! The companion driver is `_scratch/plugin-scale/run.ps1`, which points
//! `PIR_AGENT_DIR` at a scratch directory and disables the on-disk bytecode
//! cache so each N is measured cold (cache hits would otherwise hide
//! transpile/compile work, and the process-global cache is capped at 512
//! entries, so a 500-plugin run would evict earlier measurements' blobs).

use std::path::{Path, PathBuf};
use std::time::Instant;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::json;

/// Measured plugin counts for the accepted 100-plugin scale target.
const COUNTS: &[usize] = &[1, 10, 100];

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("scale-model".to_string()),
        session_id: Some("plugin-scale".to_string()),
        system_prompt: "You are a scale benchmark.".to_string(),
        is_idle: true,
        // Explicit defaults keep a developer's global settings / PI_EXT_*
        // overrides out of the measurement.
        limits: Some(Default::default()),
    }
}

/// `_shared/common.ts` is imported by every plugin entry but is NOT an
/// extension entry itself (its directory has no package.json / index file),
/// so QuickJS caches exactly one module record for it.
const SHARED_SOURCE: &str = r#"
import { createHash } from "node:crypto";

export const SHARED_TAG = "scale-shared-v1";

export function sharedStamp(input: string): string {
  return createHash("sha1").update(SHARED_TAG + ":" + input).digest("hex").slice(0, 12);
}
"#;

/// One package: manifest, entry and a per-plugin local module.
fn plugin_entry_source(index: usize, n: usize) -> String {
    let name = format!("plugin-{index:04}-n{n}");
    let tool = format!("scale_tool_{index:04}");
    let command = format!("scale_cmd_{index:04}");
    format!(
        r#"
import {{ createHash }} from "node:crypto";
import {{ basename, join }} from "node:path";
import {{ readFileSync }} from "node:fs";
import type {{ ExtensionAPI }} from "@earendil-works/pi-coding-agent";
import {{ sharedStamp }} from "../_shared/common.js";
import {{ LOCAL_TAG }} from "./local.js";

interface ScaleParams {{
  message: string;
}}

const PLUGIN_INDEX: number = {index};
const PLUGIN_NAME = "{name}";
const SCALE_N = {n};

export default function scalePlugin(pi: ExtensionAPI) {{
  // Real fs/path/crypto work in the factory: the manifest is read through the
  // shim, and the plugin identity feeds both registrations.
  const manifest = JSON.parse(readFileSync(join(import.meta.dirname, "package.json"), "utf8"));
  const marker = basename(import.meta.dirname) + ":" + manifest.version;
  const localTag: string = LOCAL_TAG + ":" + PLUGIN_NAME + ":" + SCALE_N;

  pi.registerCommand("{command}", {{
    description: "Scale fixture command for " + PLUGIN_NAME,
    handler: async () => {{
      globalThis.__scale_last_command = PLUGIN_NAME;
    }},
  }});

  pi.registerTool({{
    name: "{tool}",
    label: "Scale Tool {index:04}",
    description: "Synthetic scale fixture tool " + PLUGIN_NAME,
    parameters: {{
      type: "object",
      properties: {{
        message: {{ type: "string", description: "Message to stamp" }},
      }},
      required: ["message"],
      additionalProperties: false,
    }},
    async execute(_toolCallId: string, params: ScaleParams) {{
      const message = String(params.message);
      const local = createHash("sha256").update(localTag + ":" + message).digest("hex").slice(0, 12);
      return {{
        content: [
          {{
            type: "text",
            text: marker + ":" + sharedStamp(message) + ":" + local + ":" + message,
          }},
        ],
        details: {{ plugin: PLUGIN_NAME, index: PLUGIN_INDEX, scale: SCALE_N, manifest: manifest.name }},
      }};
    }},
  }});
}}
"#
    )
}

fn plugin_local_source(index: usize, n: usize) -> String {
    format!(
        "// Per-plugin private module (a distinct QuickJS module record per plugin).\nexport const LOCAL_TAG = \"local-{index:04}-n{n}\";\nexport function localBias(): number {{ return {index}; }}\n"
    )
}

/// Create `<root>/_shared/common.ts` plus `count` plugin directories. Returns
/// the extension root to pass to `ExtensionRuntime::load`.
fn write_scale_fixtures(root: &Path, count: usize) -> PathBuf {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).expect("create scale root");
    let shared_dir = root.join("_shared");
    std::fs::create_dir_all(&shared_dir).expect("shared dir");
    std::fs::write(shared_dir.join("common.ts"), SHARED_SOURCE).expect("shared module");

    for index in 0..count {
        let dir = root.join(format!("plugin-{index:04}-n{count}"));
        std::fs::create_dir_all(&dir).expect("plugin dir");
        std::fs::write(
            dir.join("package.json"),
            format!(
                "{{\"name\":\"{0}\",\"version\":\"1.0.0\",\"pi\":{{\"extensions\":[\"./index.ts\"]}}}}\n",
                dir.file_name().unwrap().to_string_lossy()
            ),
        )
        .expect("package manifest");
        std::fs::write(dir.join("local.ts"), plugin_local_source(index, count))
            .expect("local module");
        std::fs::write(dir.join("index.ts"), plugin_entry_source(index, count)).expect("entry");
    }
    root.to_path_buf()
}

// ---------------------------------------------------------------------------
// Process memory (Windows): peak commit (PeakPagefileUsage), peak working set
// and the CURRENT working set / private bytes. The peak counters are
// monotonic for the process; the current counters show the live footprint of
// one runtime at a time. Non-Windows runs report null for all four.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct ProcMem {
    peak_commit_bytes: usize,
    commit_bytes: usize,
    peak_working_set_bytes: usize,
    working_set_bytes: usize,
    private_bytes: usize,
}

impl ProcMem {
    fn to_json(self) -> serde_json::Value {
        json!({
            "peak_commit_bytes": self.peak_commit_bytes,
            "commit_bytes": self.commit_bytes,
            "peak_working_set_bytes": self.peak_working_set_bytes,
            "working_set_bytes": self.working_set_bytes,
            "private_bytes": self.private_bytes,
        })
    }
}

#[cfg(windows)]
fn process_memory_now() -> Option<ProcMem> {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    #[allow(unsafe_code)]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }

    let size = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    let mut counters = std::mem::MaybeUninit::<ProcessMemoryCounters>::zeroed();
    // SAFETY: `PROCESS_MEMORY_COUNTERS_EX` layout above matches the Windows
    // header, `size` is its full size, and `GetCurrentProcess` returns the
    // pseudo-handle for this process (never owned, never closed). A zeroed
    // instance is a valid value of this all-integer struct, so writing `cb`
    // before the call is sound.
    #[allow(unsafe_code)]
    unsafe {
        (*counters.as_mut_ptr()).cb = size;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), counters.as_mut_ptr(), size) == 0 {
            return None;
        }
        let counters = counters.assume_init();
        Some(ProcMem {
            peak_commit_bytes: counters.peak_pagefile_usage,
            commit_bytes: counters.pagefile_usage,
            peak_working_set_bytes: counters.peak_working_set_size,
            working_set_bytes: counters.working_set_size,
            private_bytes: counters.private_usage,
        })
    }
}

#[cfg(not(windows))]
fn process_memory_now() -> Option<ProcMem> {
    None
}

fn process_memory_json() -> serde_json::Value {
    process_memory_now()
        .map(ProcMem::to_json)
        .unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// QuickJS heap counters from inside the runtime (the same JS_ComputeMemoryUsage
// walk `node:v8`'s getHeapStatistics uses).
// ---------------------------------------------------------------------------

async fn heap_now(rt: &ExtensionRuntime) -> serde_json::Value {
    match rt
        .eval_json("JSON.stringify(globalThis.__pir_host.v8HeapStatistics())")
        .await
    {
        Ok(value) => {
            assert!(
                value
                    .get("mallocSize")
                    .and_then(serde_json::Value::as_f64)
                    .is_some(),
                "v8HeapStatistics missing mallocSize: {value}"
            );
            value
        }
        Err(error) => panic!("v8HeapStatistics failed: {error}"),
    }
}

fn bytecode_cache_disabled() -> bool {
    matches!(
        std::env::var("PIR_EXT_BYTECODE_CACHE").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

struct Measurement {
    n: usize,
    construct_ms: f64,
    load_ms: f64,
    call_all_ms: f64,
    loaded: usize,
    errors: usize,
    tools: usize,
    commands: usize,
    heap_before_load: serde_json::Value,
    heap_after_load: serde_json::Value,
    heap_after_calls: serde_json::Value,
    proc_before: serde_json::Value,
    proc_after_construct: serde_json::Value,
    proc_after_load: serde_json::Value,
    proc_after_calls: serde_json::Value,
}

impl Measurement {
    fn to_json(&self) -> serde_json::Value {
        let per_plugin_load_ms = if self.n == 0 {
            0.0
        } else {
            self.load_ms / self.n as f64
        };
        let per_plugin_call_us = if self.n == 0 {
            0.0
        } else {
            self.call_all_ms * 1000.0 / self.n as f64
        };
        json!({
            "n": self.n,
            "construct_ms": self.construct_ms,
            "load_ms": self.load_ms,
            "call_all_ms": self.call_all_ms,
            "per_plugin_load_ms": per_plugin_load_ms,
            "per_plugin_call_us": per_plugin_call_us,
            "loaded": self.loaded,
            "errors": self.errors,
            "tools": self.tools,
            "commands": self.commands,
            "heap_before_load": self.heap_before_load,
            "heap_after_load": self.heap_after_load,
            "heap_after_calls": self.heap_after_calls,
            "proc_before": self.proc_before,
            "proc_after_construct": self.proc_after_construct,
            "proc_after_load": self.proc_after_load,
            "proc_after_calls": self.proc_after_calls,
        })
    }
}

async fn measure(n: usize, root: &Path) -> Measurement {
    let root = write_scale_fixtures(root, n);
    let config = test_config_at(&root);

    let proc_before = process_memory_json();
    let started = Instant::now();
    let rt = ExtensionRuntime::new(config)
        .await
        .expect("runtime construction");
    let construct_ms = ms(started);
    let heap_before_load = heap_now(&rt).await;
    let proc_after_construct = process_memory_json();

    let load_started = Instant::now();
    let loaded = rt.load(std::slice::from_ref(&root)).await.expect("load");
    let load_ms = ms(load_started);
    assert!(
        loaded.errors.is_empty(),
        "n={n}: load errors: {:?}",
        loaded.errors
    );
    assert_eq!(loaded.loaded.len(), n, "n={n}: loaded count");
    let heap_after_load = heap_now(&rt).await;
    let proc_after_load = process_memory_json();

    let tools = rt.tool_defs();
    let commands = rt.command_defs();
    assert_eq!(tools.len(), n, "n={n}: one registered tool per plugin");
    assert_eq!(
        commands.len(),
        n,
        "n={n}: one registered command per plugin"
    );

    // Prove behavior: every plugin's registered tool answers once, with its
    // own identity and the message intact (the shared module is consulted by
    // each execution too).
    let call_started = Instant::now();
    let mut results = Vec::with_capacity(n);
    for index in 0..n {
        let tool = format!("scale_tool_{index:04}");
        let message = format!("payload-{index}");
        let result = rt
            .call_tool(
                &tool,
                &format!("scale-call-{index}"),
                &json!({ "message": message }),
            )
            .await
            .unwrap_or_else(|error| panic!("n={n}: {tool} call failed: {error}"));
        results.push((index, tool, message, result));
    }
    let call_all_ms = ms(call_started);
    for (index, tool, message, result) in results {
        assert_eq!(result.content.len(), 1, "n={n}: {tool} content count");
        let text = &result.content[0].text;
        let expected_prefix = format!("plugin-{index:04}-n{n}:1.0.0:");
        assert!(
            text.starts_with(&expected_prefix),
            "n={n}: {tool} answered {text:?}, expected prefix {expected_prefix:?}"
        );
        assert!(
            text.ends_with(&format!(":{message}")),
            "n={n}: {tool} lost the message in {text:?}"
        );
        assert_eq!(
            result.details["plugin"],
            json!(format!("plugin-{index:04}-n{n}")),
            "n={n}: {tool} details"
        );
    }
    let heap_after_calls = heap_now(&rt).await;
    let proc_after_calls = process_memory_json();

    // Drop this generation before the next N so the live footprint reported
    // for the next runtime is not the previous runtime's peak overlap.
    drop(rt);
    tokio::task::yield_now().await;
    if std::env::var("PIR_PLUGIN_SCALE_KEEP").ok().as_deref() != Some("1") {
        let _ = std::fs::remove_dir_all(&root);
    }

    Measurement {
        n,
        construct_ms,
        load_ms,
        call_all_ms,
        loaded: loaded.loaded.len(),
        errors: loaded.errors.len(),
        tools: tools.len(),
        commands: commands.len(),
        heap_before_load,
        heap_after_load,
        heap_after_calls,
        proc_before,
        proc_after_construct,
        proc_after_load,
        proc_after_calls,
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "release plugin-scale benchmark; run with --ignored --nocapture"]
async fn plugin_scale() {
    if cfg!(debug_assertions) {
        eprintln!("skipping plugin_scale in a debug profile (transpile/compile dominates)");
        return;
    }

    assert!(
        bytecode_cache_disabled(),
        "cold plugin-scale baseline requires PIR_EXT_BYTECODE_CACHE=0"
    );
    let base = std::env::var_os("PIR_PLUGIN_SCALE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("pir-plugin-scale-{}", std::process::id()))
        });
    std::fs::create_dir_all(&base).expect("scale dir");

    println!(
        "PLUGIN_SCALE_ENV {}",
        json!({
            "pid": std::process::id(),
            "dir": base.display().to_string(),
            "agent_dir": std::env::var("PIR_AGENT_DIR").ok(),
            "bytecode_cache_disabled": bytecode_cache_disabled(),
            "bytecode_shims_disabled": matches!(
                std::env::var("PIR_EXT_BYTECODE").ok().as_deref(),
                Some("0") | Some("false") | Some("off")
            ),
            "profile": option_env!("PROFILE").unwrap_or("unknown"),
            "counts": COUNTS,
        })
    );

    let mut measurements = Vec::new();
    for (slot, n) in COUNTS.iter().copied().enumerate() {
        let root = base.join(format!("plugins-{n:04}"));
        let measurement = measure(n, &root).await;
        println!("PLUGIN_SCALE {}", measurement.to_json());
        measurements.push(measurement);
        // Keep the fixture slot count bounded across a long local session.
        let _ = slot;
    }

    // Aggregate line: per-plugin deltas make super-linear growth visible at a
    // glance (`per_plugin_*` should stay roughly flat; a rising value is a
    // scaling bug, not a constant factor).
    let summary: Vec<serde_json::Value> = measurements
        .iter()
        .map(|m| {
            let load_delta = |before: &serde_json::Value, after: &serde_json::Value, key: &str| {
                let b = before.get(key).and_then(serde_json::Value::as_f64);
                let a = after.get(key).and_then(serde_json::Value::as_f64);
                match (b, a) {
                    (Some(b), Some(a)) if m.n > 0 => Some((a - b) / m.n as f64),
                    _ => None,
                }
            };
            json!({
                "n": m.n,
                "per_plugin_load_ms": m.load_ms / m.n as f64,
                "per_plugin_call_us": m.call_all_ms * 1000.0 / m.n as f64,
                "per_plugin_js_malloc_bytes": load_delta(
                    &m.heap_before_load,
                    &m.heap_after_load,
                    "mallocSize",
                ),
                "per_plugin_js_used_bytes": load_delta(
                    &m.heap_before_load,
                    &m.heap_after_load,
                    "memoryUsedSize",
                ),
                "per_plugin_private_bytes": load_delta(
                    &m.proc_before,
                    &m.proc_after_calls,
                    "private_bytes",
                ),
            })
        })
        .collect();
    println!("PLUGIN_SCALE_SUMMARY {}", serde_json::Value::Array(summary));

    if std::env::var("PIR_PLUGIN_SCALE_KEEP").ok().as_deref() != Some("1") {
        let _ = std::fs::remove_dir_all(&base);
    }
}
