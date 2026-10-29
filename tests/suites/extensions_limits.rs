//! Configurable QuickJS resource limits for the extension runtime:
//! the `extensionLimits` settings object, the `PI_EXT_*` environment
//! overrides, and the `ExtensionConfig::limits` explicit injection.
//!
//! Resolution semantics pinned here:
//! - defaults are the low-resource set: a 128 MiB heap cap (a runaway
//!   extension fails its call instead of OOM-killing the host), 16 MiB stack,
//!   30 s call timeout, 30 min hook timeout, QuickJS-default GC pacing;
//! - `0` means "no limit" for `memoryMb`/`gcThresholdMb` (and for the
//!   `PI_EXT_MEMORY_MB`/`PI_EXT_GC_THRESHOLD_MB` spellings);
//! - `stackMb` 0 keeps the default and values above 16 MiB are clamped (a
//!   larger value would turn off QuickJS's stack check); timeout 0 keeps the
//!   default;
//! - invalid env values are ignored and reported as diagnostics;
//! - an explicit `Some(ExtensionLimits)` is adopted verbatim;
//! - project-scope `.pi/settings.json` can never loosen the limits: the
//!   engine reads the GLOBAL settings file only.
//!
//! The env parsing is exercised through the pure
//! `ExtensionLimits::apply_env` seam instead of mutating the process
//! environment: this binary (the merged `g_extensions` runner) runs suites
//! concurrently, and a process-global `PI_EXT_*` write would leak into every
//! other suite's runtime. The E2E tests therefore inject limits explicitly —
//! the same `ExtensionConfig::limits` value a host resolves from
//! settings+env.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use pir::config::settings::{Settings, SettingsManager};
use pir::extensions::limits::global_settings_from_dir;
use pir::extensions::{
    ExtensionConfig, ExtensionHandle, ExtensionLimits, DEFAULT_BEFORE_AGENT_START_TIMEOUT,
    DEFAULT_CALL_TIMEOUT, HOOK_CALL_TIMEOUT,
};

const MB: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ext-limits-{label}-{}-{}",
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

fn settings_from(value: serde_json::Value) -> Settings {
    serde_json::from_value(value).expect("settings must deserialize")
}

fn injected_config(cwd: &Path, limits: ExtensionLimits) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(limits),
    }
}

async fn load_extension(dir: &TempDir, source: &str, limits: ExtensionLimits) -> ExtensionHandle {
    let path = dir.0.join("ext.ts");
    std::fs::write(&path, source).expect("write fixture");
    let handle = ExtensionHandle::start(injected_config(&dir.0, limits)).expect("runtime");
    let result = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    assert_eq!(result.loaded.len(), 1, "fixture must load");
    handle
}

// ---------------------------------------------------------------------------
// Resolution: defaults, settings, env, explicit injection
// ---------------------------------------------------------------------------

#[test]
fn defaults_use_the_low_resource_heap_cap_and_stack() {
    let limits = ExtensionLimits::default();
    assert_eq!(
        limits.memory_bytes,
        Some(pir::extensions::limits::DEFAULT_MEMORY_BYTES),
        "128 MiB heap cap by default"
    );
    assert_eq!(limits.memory_bytes, Some(128 * MB));
    assert_eq!(limits.stack_bytes, 16 * MB, "16 MiB JS stack");
    assert_eq!(limits.gc_threshold_bytes, None, "QuickJS GC pacing");
    assert_eq!(limits.call_timeout, DEFAULT_CALL_TIMEOUT);
    assert_eq!(limits.call_timeout, Duration::from_secs(30));
    assert_eq!(limits.hook_timeout, HOOK_CALL_TIMEOUT);
    assert_eq!(limits.hook_timeout, Duration::from_secs(30 * 60));
    assert!(
        limits.summary().contains("memory=128 MB"),
        "{}",
        limits.summary()
    );
    assert!(limits.summary().contains("stack=16 MB"));
    // The actionable failure text names the cap and both escape hatches.
    let message = limits.memory_limit_error_message();
    assert!(message.contains("128 MB"), "{message}");
    assert!(message.contains("extensionLimits.memoryMb"), "{message}");
    assert!(message.contains("PI_EXT_MEMORY_MB"), "{message}");
    assert!(message.contains("0 = no limit"), "{message}");
}

#[test]
fn explicit_zero_still_disables_the_default_heap_cap() {
    // `memoryMb: 0` and `PI_EXT_MEMORY_MB=0` are the documented escape
    // hatches back to QuickJS's unlimited heap.
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(json!({
        "extensionLimits": { "memoryMb": 0 }
    })));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.memory_bytes, None);

    let (limits, diagnostics) = ExtensionLimits::default().apply_env(&[("PI_EXT_MEMORY_MB", "0")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.memory_bytes, None);

    let (limits, diagnostics) =
        ExtensionLimits::default().apply_env(&[("PI_EXT_MEMORY_MB", "512")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.memory_bytes, Some(512 * MB));
}

#[test]
fn actor_thread_stack_is_derived_from_the_js_stack_budget() {
    let defaults = ExtensionLimits::default();
    assert_eq!(
        defaults.actor_stack_bytes(),
        2 * defaults.stack_bytes + pir::extensions::limits::ACTOR_STACK_HEADROOM_BYTES
    );
    assert_eq!(defaults.actor_stack_bytes(), 40 * MB);
    assert!(
        defaults.actor_stack_bytes() < 64 * MB,
        "no longer a flat 64 MiB reservation per extension"
    );
    assert!(
        defaults.actor_stack_bytes() >= defaults.stack_bytes,
        "the OS stack must not be smaller than the JS budget"
    );

    // A low-resource operator lowering the JS stack (PI_EXT_STACK_MB=8 /
    // extensionLimits.stackMb: 8) shrinks the actor reservation too.
    let low = ExtensionLimits {
        stack_bytes: 8 * MB,
        ..ExtensionLimits::default()
    };
    assert_eq!(low.actor_stack_bytes(), 24 * MB);
    assert!(low.actor_stack_bytes() >= low.stack_bytes);
}

#[test]
fn settings_object_maps_to_limits_and_zero_means_no_limit() {
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(json!({
        "extensionLimits": {
            "memoryMb": 64,
            "stackMb": 8,
            "gcThresholdMb": 4,
            "callTimeoutMs": 1500,
            "hookTimeoutMs": 60_000,
            "beforeAgentStartTimeoutMs": 2500,
        }
    })));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.memory_bytes, Some(64 * MB));
    assert_eq!(limits.stack_bytes, 8 * MB);
    assert_eq!(limits.gc_threshold_bytes, Some(4 * MB));
    assert_eq!(limits.call_timeout, Duration::from_millis(1500));
    assert_eq!(limits.hook_timeout, Duration::from_millis(60_000));
    assert_eq!(
        limits.before_agent_start_timeout,
        Duration::from_millis(2500)
    );

    // 0 = no limit for the heap cap and the GC threshold.
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(json!({
        "extensionLimits": { "memoryMb": 0, "gcThresholdMb": 0 }
    })));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.memory_bytes, None);
    assert_eq!(limits.gc_threshold_bytes, None);
}

#[test]
fn stack_requests_are_clamped_and_zero_keeps_the_default() {
    // Above 16 MiB rquickjs would read the value as "no stack check", which
    // can crash the process on deep recursion: the resolver clamps instead.
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(json!({
        "extensionLimits": { "stackMb": 64 }
    })));
    assert_eq!(limits.stack_bytes, 16 * MB);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("clamped"));

    // stackMb 0 keeps the 16 MiB default (with a diagnostic).
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(json!({
        "extensionLimits": { "stackMb": 0 }
    })));
    assert_eq!(limits.stack_bytes, 16 * MB);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");

    // Timeout 0 keeps the defaults too (the interrupt machinery always needs
    // a positive deadline).
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(json!({
        "extensionLimits": { "callTimeoutMs": 0, "hookTimeoutMs": 0 }
    })));
    assert_eq!(limits.call_timeout, DEFAULT_CALL_TIMEOUT);
    assert_eq!(limits.hook_timeout, HOOK_CALL_TIMEOUT);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");

    // The before_agent_start lifecycle budget has its own conservative
    // default (10 s) and the same 0-keeps-the-default rule.
    let (limits, diagnostics) = ExtensionLimits::from_settings(&settings_from(
        json!({ "extensionLimits": { "beforeAgentStartTimeoutMs": 0 } }),
    ));
    assert_eq!(
        limits.before_agent_start_timeout,
        pir::extensions::DEFAULT_BEFORE_AGENT_START_TIMEOUT
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
}

#[test]
fn worker_stack_request_never_disables_the_engine_stack_check() {
    // rquickjs-core 0.14.0 `runtime/raw.rs::set_max_stack_size`:
    //   const MAX_SANE: usize = 1 << 24;
    //   let limit: size_t = if limit > MAX_SANE { 0 } else { ... };
    // and QuickJS `update_stack_limit` reads 0 as `stack_limit = 0`, i.e.
    // "no limit": an oversized request SILENTLY turns the recursion guard
    // off. The worker runtime used to ask for 32 MiB (above the cutoff), so
    // its stack check was voided and deep recursion could abort the process
    // instead of throwing `RangeError`. Pin both the worker's request and
    // the engine cutoff with literals (rquickjs does not export MAX_SANE),
    // so raising either fails here and at the `const` assertion in
    // `worker_fns.rs`.
    const RQUICKJS_MAX_SANE: usize = 1 << 24;
    let requested = pir::extensions::runtime::WORKER_JS_STACK_BYTES;
    assert!(
        requested <= RQUICKJS_MAX_SANE,
        "worker JS stack request {requested} exceeds rquickjs MAX_SANE \
         {RQUICKJS_MAX_SANE}: the engine would silently disable the stack check"
    );
    assert!(
        requested <= pir::extensions::limits::MAX_STACK_BYTES,
        "the worker request must stay within the crate's own clamp"
    );
    assert_eq!(
        requested,
        pir::extensions::limits::MAX_STACK_BYTES,
        "the worker runs at the crate's clamped maximum (16 MiB)"
    );
    // The pre-fix request, recorded here as a compile-time negative case: 32
    // MiB is over the engine cutoff, which is why it disabled the check.
    const _: () = assert!(32 * MB > RQUICKJS_MAX_SANE);
}

#[test]
fn env_overrides_beat_settings_and_invalid_values_are_ignored() {
    let base = ExtensionLimits {
        memory_bytes: Some(64 * MB),
        call_timeout: Duration::from_secs(5),
        ..ExtensionLimits::default()
    };
    let (limits, diagnostics) = base.clone().apply_env(&[
        ("PI_EXT_MEMORY_MB", "256"),
        ("PI_EXT_STACK_MB", "8"),
        ("PI_EXT_GC_THRESHOLD_MB", "0"),
        ("PI_EXT_CALL_TIMEOUT_MS", "2500"),
        ("PI_EXT_HOOK_TIMEOUT_MS", "later"),
        ("UNRELATED", "1"),
    ]);
    assert_eq!(limits.memory_bytes, Some(256 * MB), "env beats settings");
    assert_eq!(limits.stack_bytes, 8 * MB);
    assert_eq!(limits.gc_threshold_bytes, None, "0 = no threshold");
    assert_eq!(limits.call_timeout, Duration::from_millis(2500));
    assert_eq!(
        limits.hook_timeout, HOOK_CALL_TIMEOUT,
        "an invalid value leaves the field untouched"
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("PI_EXT_HOOK_TIMEOUT_MS"));
    assert!(diagnostics[0].contains("ignored"));

    // PI_EXT_MEMORY_MB=0 disables a cap configured in settings.
    let (limits, _) = base.clone().apply_env(&[("PI_EXT_MEMORY_MB", "0")]);
    assert_eq!(limits.memory_bytes, None);

    // PI_EXT_STACK_MB=0 is refused (would disable the stack check).
    let (limits, diagnostics) = base.clone().apply_env(&[("PI_EXT_STACK_MB", "0")]);
    assert_eq!(limits.stack_bytes, 16 * MB);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");

    // A non-numeric memory value is ignored, the previous value stands.
    let (limits, diagnostics) = base.apply_env(&[("PI_EXT_MEMORY_MB", "lots")]);
    assert_eq!(limits.memory_bytes, Some(64 * MB));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
}

#[test]
fn explicit_injection_is_adopted_verbatim() {
    let injected = ExtensionLimits {
        memory_bytes: Some(32 * MB),
        stack_bytes: 4 * MB,
        gc_threshold_bytes: Some(2 * MB),
        call_timeout: Duration::from_millis(750),
        hook_timeout: Duration::from_millis(9000),
        before_agent_start_timeout: DEFAULT_BEFORE_AGENT_START_TIMEOUT,
    };
    // `resolve(Some(..))` never consults settings or the environment (the
    // SDK/embedder/test seam this suite's E2E tests use).
    assert_eq!(
        ExtensionLimits::resolve(Some(injected.clone())),
        injected,
        "explicit injection wins verbatim"
    );
}

#[test]
fn project_settings_cannot_loosen_engine_limits() {
    let global = TempDir::new("global");
    let project = TempDir::new("project");
    std::fs::write(
        global.0.join("settings.json"),
        r#"{"extensionLimits":{"memoryMb":512}}"#,
    )
    .expect("write global settings");
    std::fs::write(
        project.0.join("settings.json"),
        r#"{"extensionLimits":{"memoryMb":0}}"#,
    )
    .expect("write project settings");

    // The merged trusted view does see the project value (0 = no cap) ...
    let merged = SettingsManager::load(&global.0, &project.0, &project.0, true);
    assert_eq!(
        merged
            .settings
            .extension_limits
            .as_ref()
            .and_then(|limits| limits.memory_mb),
        Some(0),
        "sanity: the project file would loosen the cap if it were consulted"
    );

    // ... but the engine resolver reads the global file only.
    let (limits, diagnostics) =
        ExtensionLimits::from_settings(&global_settings_from_dir(&global.0));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(
        limits.memory_bytes,
        Some(512 * MB),
        "project settings must not loosen the engine limits"
    );

    // A missing global file resolves to the defaults.
    let empty = TempDir::new("missing-global");
    let (limits, diagnostics) = ExtensionLimits::from_settings(&global_settings_from_dir(&empty.0));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits, ExtensionLimits::default());
}

// ---------------------------------------------------------------------------
// E2E: the heap cap contains a runaway extension
// ---------------------------------------------------------------------------

const MEMORY_HOG_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "memoryHog",
    description: "allocates until the configured heap cap refuses",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const chunks = [];
      for (let i = 0; i < 8192; i++) {
        chunks.push(new Uint8Array(1024 * 1024));
      }
      return { content: [{ type: "text", text: "unreachable" }] };
    },
  });
  pi.registerTool({
    name: "ping",
    description: "still alive after the cap tripped",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "pong" }] }),
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn heap_cap_fails_the_call_with_actionable_error_and_host_survives() {
    let dir = TempDir::new("heap-cap");
    // 256 MiB: comfortably above the runtime's boot footprint (so the load
    // itself succeeds) and far below the tool body's 8 GiB attempt, so the
    // allocation MUST be refused at the cap. The exact number is incidental;
    // the contract is "the cap contains the call, the host survives".
    let limits = ExtensionLimits {
        memory_bytes: Some(256 * MB),
        ..ExtensionLimits::default()
    };
    let handle = load_extension(&dir, MEMORY_HOG_TS, limits).await;

    let started = Instant::now();
    let error = handle
        .call_tool("memoryHog", "call-1", &json!({}))
        .await
        .expect_err("an over-cap tool call must fail");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the call must stop at the cap, took {:?}",
        started.elapsed()
    );
    let text = error.to_string();
    assert!(
        text.contains("exceeded its JS memory limit (256 MB)"),
        "the failure must name the cap: {text}"
    );
    assert!(
        text.contains("extensionLimits.memoryMb"),
        "the failure must name the setting: {text}"
    );
    assert!(
        text.contains("PI_EXT_MEMORY_MB"),
        "the failure must name the env override: {text}"
    );

    // The host survives the cap and the runtime keeps working: a subsequent
    // tool call on the same extension succeeds ("one extension cannot take
    // the application down").
    let ok = handle
        .call_tool("ping", "call-2", &json!({}))
        .await
        .expect("the runtime must stay usable after a heap-limit hit");
    assert_eq!(ok.content[0].text, "pong");
}

// ---------------------------------------------------------------------------
// E2E: configured call/hook timeouts reach the interrupt machinery
// ---------------------------------------------------------------------------

const TIMEOUT_TS: &str = r#"
export default function (pi) {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  pi.registerTool({
    name: "spin",
    description: "awaits far past the configured call timeout",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      await sleep(30000);
      return { content: [{ type: "text", text: "late" }] };
    },
  });
  pi.registerTool({
    name: "ping",
    description: "usable after the timeout",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "pong" }] }),
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn configured_call_timeout_bounds_tool_calls() {
    let dir = TempDir::new("call-timeout");
    let limits = ExtensionLimits {
        call_timeout: Duration::from_millis(250),
        ..ExtensionLimits::default()
    };
    let handle = load_extension(&dir, TIMEOUT_TS, limits).await;

    let started = Instant::now();
    let error = handle
        .call_tool("spin", "call-1", &json!({}))
        .await
        .expect_err("the configured call timeout must abort the sleep");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the 250 ms budget must apply (the default is 30 s), took {:?}",
        started.elapsed()
    );
    assert!(
        error.to_string().contains("timed out"),
        "timeout error: {error}"
    );

    // The interrupt path leaves the runtime usable (same recovery as the
    // heap cap).
    let ok = handle
        .call_tool("ping", "call-2", &json!({}))
        .await
        .expect("runtime usable after the configured timeout");
    assert_eq!(ok.content[0].text, "pong");
}

const HOOK_TS: &str = r#"
export default function (pi) {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  pi.registerProvider("limitsprov", {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: "limits-model", name: "Limits model", contextWindow: 1000, maxTokens: 100, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    streamSimple: async () => {
      await sleep(30000);
      return [];
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn configured_hook_timeout_bounds_reserved_prefix_hooks() {
    let dir = TempDir::new("hook-timeout");
    let limits = ExtensionLimits {
        // The reserved-prefix path must use `hookTimeoutMs`, not the
        // (10 s) per-call timeout: the call below has to abort at 300 ms.
        call_timeout: Duration::from_secs(10),
        hook_timeout: Duration::from_millis(300),
        ..ExtensionLimits::default()
    };
    let handle = load_extension(&dir, HOOK_TS, limits).await;

    let started = Instant::now();
    let error = handle
        .call_prepare_loadout("__pir_stream:limitsprov", &json!({}))
        .await
        .expect_err("the configured hook timeout must abort the streamSimple callback");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the 300 ms hook budget must apply, took {:?}",
        started.elapsed()
    );
    assert!(error.contains("timed out"), "hook timeout error: {error}");
}

// ---------------------------------------------------------------------------
// Codemode sandbox heap cap (`codemode.sandboxMemoryMb`)
// ---------------------------------------------------------------------------

#[test]
fn codemode_stack_is_derived_from_the_js_stack_budget() {
    // The sandbox thread's OS stack is no longer a flat 64 MiB reservation: it
    // derives from the JS stack with the extension actor's formula (2x + the
    // native headroom in limits.rs), so the OS stack always covers the JS
    // budget. Only constants/formulas are pinned here — the limits.rs behavior
    // is untouched.
    use pir::plugins::codemode::{sandbox_thread_stack_bytes, SANDBOX_JS_STACK_BYTES};

    assert_eq!(
        SANDBOX_JS_STACK_BYTES,
        16 * MB,
        "the sandbox JS stack is rquickjs's clamped maximum"
    );
    assert_eq!(
        SANDBOX_JS_STACK_BYTES,
        pir::extensions::limits::MAX_STACK_BYTES
    );
    let headroom = pir::extensions::limits::ACTOR_STACK_HEADROOM_BYTES;
    assert_eq!(
        sandbox_thread_stack_bytes(),
        2 * SANDBOX_JS_STACK_BYTES + headroom,
        "the OS stack derives from the JS stack budget (2x + native headroom)"
    );
    assert_eq!(sandbox_thread_stack_bytes(), 40 * MB);
    assert!(sandbox_thread_stack_bytes() >= SANDBOX_JS_STACK_BYTES);
    assert!(
        sandbox_thread_stack_bytes() < 64 * MB,
        "no flat 64 MiB reservation per sandbox run"
    );
    // Same formula as `ExtensionLimits::actor_stack_bytes` (limits.rs), so the
    // sandbox and actor thread budgets cannot drift apart.
    let equivalent = ExtensionLimits {
        stack_bytes: SANDBOX_JS_STACK_BYTES,
        ..ExtensionLimits::default()
    };
    assert_eq!(sandbox_thread_stack_bytes(), equivalent.actor_stack_bytes());
}

#[test]
fn codemode_sandbox_memory_setting_maps_to_the_engine_cap() {
    // Default: 128 MiB (the value both sandbox engines used to hardcode).
    let empty = TempDir::new("codemode-default");
    let manager = SettingsManager::load(&empty.0, &empty.0, &empty.0, false);
    assert_eq!(
        pir::plugins::codemode::codemode_config_from(&manager).sandbox_memory_bytes,
        Some(128 * MB)
    );

    // Configured MiB.
    let global = TempDir::new("codemode-memory");
    std::fs::write(
        global.0.join("settings.json"),
        r#"{"codemode":{"sandboxMemoryMb":64}}"#,
    )
    .expect("write settings");
    let manager = SettingsManager::load(&global.0, &global.0, &global.0, false);
    assert_eq!(
        pir::plugins::codemode::codemode_config_from(&manager).sandbox_memory_bytes,
        Some(64 * MB)
    );

    // 0 = no cap.
    std::fs::write(
        global.0.join("settings.json"),
        r#"{"codemode":{"sandboxMemoryMb":0}}"#,
    )
    .expect("write settings");
    let manager = SettingsManager::load(&global.0, &global.0, &global.0, false);
    assert_eq!(
        pir::plugins::codemode::codemode_config_from(&manager).sandbox_memory_bytes,
        None
    );
}
