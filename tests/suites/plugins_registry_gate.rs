//! The plugin host's registry gate: rebuild-exactly-when-changed, and the
//! O(1) half that skips the deep definition read while nothing moved.
//!
//! Pins four things:
//!
//! 1. **The guarantee.** A definition written by a JS handler DURING a
//!    dispatch is visible in the shared registry by the time that `emit`
//!    returns (add / replace / remove), because the tail-of-emit check
//!    compares the JS fingerprints after the runtime re-exported them.
//! 2. **The fallback cost.** Without an installed generation probe every
//!    check deep-reads the definitions (the exact, pre-probe behavior — a host
//!    built without a JS runtime, or one whose probe was cleared).
//! 3. **The O(1) fast path.** With a generation probe installed — the real
//!    runtime probe `PluginHost::build` wires whenever it got a JS handle, or
//!    a test probe — a clean emit performs no deep read; a moved generation
//!    re-enables the exact half exactly once and, when the defs really
//!    changed, rebuilds — a bare generation bump does NOT rebuild (the
//!    fingerprint comparison stays the rebuild decision).
//! 4. **Order sensitivity + payload shape.** The command fingerprints are
//!    order-sensitive (so a reordered unchanged set is a detected change),
//!    and the JS-visible event payload is byte-identical to the value the host
//!    dispatched — the host's `EmitOutcome::payload` mirror is `tool_call`
//!    only, its documented contract (the mirror clone was pure per-event
//!    cost; the caller keeps the mutated payload through its own `&mut`).
//! 5. **The wired runtime probe.** A definition written through the real
//!    runtime — a `pi.registerProvider`-shaped `registerProviderJson` call or
//!    the export-less `registerFlagJson` — moves the runtime generation the
//!    host installed, so the next emit reads the fingerprints exactly once
//!    more and publishes the change, while clean emits stay at O(1).
//!
//! Fixtures follow the repo hygiene rule: JS sources are written by the test,
//! never committed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::PluginHost;
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-gate-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("plugins-gate-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Write a JS extension source and load it into a runtime actor.
async fn load_runtime(dir: &TempDir, sources: &[(&str, &str)]) -> ExtensionHandle {
    let runtime = ExtensionHandle::start(test_config_at(&dir.0)).expect("runtime");
    let mut paths = Vec::new();
    for (name, source) in sources {
        let path = dir.0.join(name);
        std::fs::write(&path, source).expect("write fixture");
        paths.push(path);
    }
    let result = runtime.load(&paths).await.expect("load extensions");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    runtime
}

/// A fixture whose `gate_probe` handler applies the change the test staged on
/// `globalThis.__stage` — so the definition write happens DURING the dispatch,
/// which is the case the gate's invariant is about.
const JS_DEF_STAGE: &str = r#"
    export default function (pi) {
        globalThis.__stage = null;
        pi.on("gate_probe", () => {
            const stage = globalThis.__stage;
            if (!stage) return;
            globalThis.__stage = null;
            if (stage.remove) pi.unregisterProvider(stage.remove);
            if (stage.add) pi.registerProvider(stage.add.name, stage.add.config);
        });
    }
"#;

/// Stage a change for the `gate_probe` handler by writing `globalThis.__stage`
/// (`eval_json` hands the value back as JSON so the write is observable).
async fn stage(runtime: &ExtensionHandle, value: &str) {
    let staged = runtime
        .eval_json(&format!("JSON.stringify(globalThis.__stage = {value})"))
        .await
        .expect("stage the pending definition change");
    assert!(staged.is_object(), "staging must produce the change object");
}

/// One `gate_probe` emit (the payload carries nothing the handler reads).
async fn emit_probe(host: &PluginHost) {
    let mut payload = json!({ "type": "gate_probe" });
    host.emit("gate_probe", &mut payload).await.expect("emit");
}

// ---------------------------------------------------------------------------
// The guarantee: a definition written during dispatch is visible at emit tail
// ---------------------------------------------------------------------------

/// Without a probe (the exact gate), a definition registered by a handler
/// DURING the dispatch is in the shared registry by the time `emit` returns —
/// and the same holds for a replaced and a removed definition.
#[tokio::test(flavor = "current_thread")]
async fn definitions_changed_during_dispatch_are_published_before_emit_returns() {
    let temp = TempDir::new("during-dispatch");
    let runtime = load_runtime(&temp, &[("gate.ts", JS_DEF_STAGE)]).await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());

    // Baseline: the fixture registers no definitions, only its handler.
    assert!(host.registry().provider_defs().is_empty());

    // ADD: the handler runs `pi.registerProvider` mid-dispatch.
    stage(
        &runtime,
        r#"{ add: { name: "gate-provider", config: { baseUrl: "http://one" } } }"#,
    )
    .await;
    emit_probe(&host).await;
    let defs = host.registry().provider_defs();
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(defs[0]["id"], json!("gate-provider"));
    assert_eq!(defs[0]["config"]["baseUrl"], json!("http://one"));

    // REPLACE: same id, new config — the config is part of the fingerprint, so
    // the rebuild must publish the new one (a name-only gate would miss it).
    stage(
        &runtime,
        r#"{ add: { name: "gate-provider", config: { baseUrl: "http://two" } } }"#,
    )
    .await;
    emit_probe(&host).await;
    let defs = host.registry().provider_defs();
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(defs[0]["config"]["baseUrl"], json!("http://two"));

    // REMOVE: the definition (and its handler) go away.
    stage(&runtime, r#"{ remove: "gate-provider" }"#).await;
    emit_probe(&host).await;
    assert!(
        host.registry().provider_defs().is_empty(),
        "removed definition must leave the registry"
    );
}

// ---------------------------------------------------------------------------
// The fallback cost and the O(1) fast path
// ---------------------------------------------------------------------------

/// No probe installed = the exact gate: every check deep-reads the definition
/// classes (the behavior the probe exists to skip, byte for byte the pre-probe
/// host). `PluginHost::build` now wires the runtime probe whenever it got a JS
/// handle, so this test clears it explicitly to reach the fallback.
#[tokio::test(flavor = "current_thread")]
async fn without_a_probe_every_check_reads_the_definitions() {
    let temp = TempDir::new("no-probe");
    let runtime = load_runtime(&temp, &[("gate.ts", JS_DEF_STAGE)]).await;
    let mut host = PluginHost::build(Some(runtime), Vec::new());
    host.set_js_def_generation_probe(None);

    let before = host.js_def_fingerprint_reads();
    for _ in 0..4 {
        emit_probe(&host).await;
    }
    assert_eq!(
        host.js_def_fingerprint_reads() - before,
        4,
        "the fallback gate reads the defs on every check (no probe installed)"
    );
}

/// With a generation probe installed, N clean emits do ONE deep read (the
/// first check after the install latches the generation). A moved generation
/// re-enables the exact half exactly once and never rebuilds on its own.
#[tokio::test(flavor = "current_thread")]
async fn a_clean_emit_with_a_generation_probe_skips_the_deep_read() {
    let temp = TempDir::new("probe-clean");
    let runtime = load_runtime(&temp, &[("gate.ts", JS_DEF_STAGE)]).await;
    let mut host = PluginHost::build(Some(runtime.clone()), Vec::new());

    // The probe stands in for the runtime-side O(1) generation read: it is the
    // value a real runtime moves on every definition write.
    let generation = Arc::new(AtomicU64::new(0));
    let probe_generation = Arc::clone(&generation);
    host.set_js_def_generation_probe(Some(Arc::new(move || {
        probe_generation.load(Ordering::SeqCst)
    })));

    let registry = host.registry();
    let before = host.js_def_fingerprint_reads();
    for _ in 0..8 {
        emit_probe(&host).await;
    }
    assert_eq!(
        host.js_def_fingerprint_reads() - before,
        1,
        "a clean emit with an installed probe must not re-read the defs"
    );
    assert!(
        Arc::ptr_eq(&registry, &host.registry()),
        "a clean emit must not rebuild the registry"
    );

    // A moved generation re-enables the exact half exactly once...
    generation.fetch_add(1, Ordering::SeqCst);
    emit_probe(&host).await;
    assert_eq!(host.js_def_fingerprint_reads() - before, 2);
    // ...and with the definitions unchanged it must NOT rebuild, and the
    // re-read re-latches the new generation (so the next emit is O(1) again).
    assert!(
        Arc::ptr_eq(&registry, &host.registry()),
        "a bare generation bump must not rebuild the registry"
    );
    emit_probe(&host).await;
    assert_eq!(
        host.js_def_fingerprint_reads() - before,
        2,
        "the re-read must latch the moved generation"
    );
}

/// With the probe installed, a real definition change accompanied by the
/// generation move it causes is still detected and rebuilt.
#[tokio::test(flavor = "current_thread")]
async fn a_real_change_with_a_probe_bump_rebuilds_the_registry() {
    let temp = TempDir::new("probe-change");
    let runtime = load_runtime(&temp, &[("gate.ts", JS_DEF_STAGE)]).await;
    let mut host = PluginHost::build(Some(runtime.clone()), Vec::new());

    let generation = Arc::new(AtomicU64::new(0));
    let probe_generation = Arc::clone(&generation);
    host.set_js_def_generation_probe(Some(Arc::new(move || {
        probe_generation.load(Ordering::SeqCst)
    })));

    emit_probe(&host).await; // latch the initial generation
    let registry = host.registry();
    assert!(registry.provider_defs().is_empty());

    // The handler registers the provider during the dispatch; the generation
    // move the runtime performs for that write is what the test models here.
    stage(
        &runtime,
        r#"{ add: { name: "gate-provider", config: { baseUrl: "http://one" } } }"#,
    )
    .await;
    generation.fetch_add(1, Ordering::SeqCst);
    emit_probe(&host).await;

    assert!(
        !Arc::ptr_eq(&registry, &host.registry()),
        "a real definition change must rebuild the registry"
    );
    let defs = host.registry().provider_defs();
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(defs[0]["id"], json!("gate-provider"));
}

// ---------------------------------------------------------------------------
// The WIRED runtime probe: definition writes through the real runtime
// ---------------------------------------------------------------------------

/// `PluginHost::build` installs the runtime's own generation
/// (`ExtensionHandle::defs_gen`) as the gate's probe. A **provider** definition
/// written through the real host fn (`registerProviderJson`, the
/// `pi.registerProvider` path) OUTSIDE any dispatch moves it, so the next emit
/// re-enables the exact read exactly once and publishes the change; a clean
/// emit stays at O(1) (no read at all).
#[tokio::test(flavor = "current_thread")]
async fn a_runtime_provider_write_moves_the_wired_generation_and_is_published() {
    let temp = TempDir::new("wired-provider");
    let runtime = load_runtime(&temp, &[("gate.ts", JS_DEF_STAGE)]).await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());

    // The build's snapshot carries no generation, so the first check deep-reads
    // once and latches the wired runtime generation; every clean emit after
    // that is O(1).
    emit_probe(&host).await;
    let latched = host.js_def_fingerprint_reads();
    let registry = host.registry();
    for _ in 0..4 {
        emit_probe(&host).await;
    }
    assert_eq!(
        host.js_def_fingerprint_reads(),
        latched,
        "a clean emit under the wired runtime probe must not re-read the defs"
    );
    assert!(
        Arc::ptr_eq(&registry, &host.registry()),
        "a clean emit under the wired runtime probe must not rebuild"
    );

    // The definition write goes through the REAL host fn, with no dispatch (and
    // so no export / dispatch-tail refresh) in between.
    let generation = runtime.defs_gen();
    runtime
        .eval_json(
            r#"
            __pir_host.registerProviderJson(JSON.stringify({ id: "rt-provider", config: { baseUrl: "http://runtime" } }));
            JSON.stringify("ok")
            "#,
        )
        .await
        .expect("register a provider through the runtime host fn");
    assert_eq!(
        runtime.defs_gen(),
        generation + 1,
        "the host fn must move the runtime's definition generation"
    );

    emit_probe(&host).await;
    assert_eq!(
        host.js_def_fingerprint_reads(),
        latched + 1,
        "the moved generation must re-enable the exact read exactly once"
    );
    let defs = host.registry().provider_defs();
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(defs[0]["id"], json!("rt-provider"));
    assert_eq!(defs[0]["config"]["baseUrl"], json!("http://runtime"));

    // The re-read latched the moved generation: the next clean emit is O(1)
    // again.
    emit_probe(&host).await;
    assert_eq!(
        host.js_def_fingerprint_reads(),
        latched + 1,
        "the re-read must latch the generation the host fn moved"
    );
}

/// The flag class has no export path at all — `registerFlagJson` writes the
/// list the host fingerprints directly — so it is the sharpest case for the
/// host-fn half of the probe contract: the write moves the wired generation,
/// the next emit reads the defs exactly once more, and the flag lands in the
/// shared registry.
#[tokio::test(flavor = "current_thread")]
async fn a_runtime_flag_write_moves_the_wired_generation_and_is_published() {
    let temp = TempDir::new("wired-flag");
    let runtime = load_runtime(&temp, &[("gate.ts", JS_DEF_STAGE)]).await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());

    fn registered_flags(host: &PluginHost) -> Vec<String> {
        host.registry()
            .plugins()
            .iter()
            .flat_map(|plugin| {
                plugin
                    .registrations
                    .flags
                    .iter()
                    .map(|flag| flag.name.clone())
            })
            .collect()
    }

    emit_probe(&host).await; // latch the wired generation
    let latched = host.js_def_fingerprint_reads();
    assert!(
        !registered_flags(&host)
            .iter()
            .any(|name| name == "gate-flag"),
        "the fixture registers no flags"
    );

    let generation = runtime.defs_gen();
    runtime
        .eval_json(
            r#"
            __pir_host.registerFlagJson(JSON.stringify({ extId: 0, name: "gate-flag", type: "string", description: "from a host fn", default: null }));
            JSON.stringify("ok")
            "#,
        )
        .await
        .expect("register a flag through the runtime host fn");
    assert_eq!(
        runtime.defs_gen(),
        generation + 1,
        "the flag host fn must move the runtime's definition generation"
    );

    emit_probe(&host).await;
    assert_eq!(
        host.js_def_fingerprint_reads(),
        latched + 1,
        "the moved generation must re-enable the exact read exactly once"
    );
    let flags = registered_flags(&host);
    assert!(
        flags.iter().any(|name| name == "gate-flag"),
        "the flag write must be published by that one re-read: {flags:?}"
    );

    // Latched again: the next clean emit is O(1).
    emit_probe(&host).await;
    assert_eq!(host.js_def_fingerprint_reads(), latched + 1);
}

// ---------------------------------------------------------------------------
// Order sensitivity (the "reordered defs" half of the guarantee)
// ---------------------------------------------------------------------------

/// Command definitions are fingerprinted in EXPORT order: duplicate command
/// names resolve to `name:2`, `name:3`, ... by occurrence, so the same
/// definition set exported in the other order is a real registry change. A
/// sorted (set-based) comparison — sound for the name-keyed classes — would
/// silently miss it, which is why the command class is compared as a vector.
#[tokio::test(flavor = "current_thread")]
async fn command_def_fingerprints_are_order_sensitive() {
    const CMD_A: &str = r#"
        export default function (pi) {
            pi.registerCommand("gate-cmd", { description: "from-a", handler: async () => {} });
        }
    "#;
    const CMD_B: &str = r#"
        export default function (pi) {
            pi.registerCommand("gate-cmd", { description: "from-b", handler: async () => {} });
        }
    "#;

    let temp = TempDir::new("order");
    let forward = load_runtime(&temp, &[("a.ts", CMD_A), ("b.ts", CMD_B)]).await;
    let reversed = load_runtime(&temp, &[("b.ts", CMD_B), ("a.ts", CMD_A)]).await;

    let forward = pir::plugins::host::command_def_fingerprints(&forward);
    let reversed = pir::plugins::host::command_def_fingerprints(&reversed);
    assert_eq!(forward.len(), 2, "{forward:?}");
    assert_ne!(
        forward, reversed,
        "the same command set in a different export order must fingerprint differently"
    );

    let mut forward_sorted = forward;
    let mut reversed_sorted = reversed;
    forward_sorted.sort();
    reversed_sorted.sort();
    assert_eq!(
        forward_sorted, reversed_sorted,
        "the two exports carry the same set — only the order moved"
    );
}

// ---------------------------------------------------------------------------
// The JS-visible payload shape and the host's outcome mirror
// ---------------------------------------------------------------------------

const JS_ECHO: &str = r#"
    export default function (pi) {
        globalThis.__seenPayload = null;
        pi.on("gate_probe", (event) => {
            globalThis.__seenPayload = JSON.stringify(event);
            event.echoed = true;
        });
        pi.on("tool_call", (event) => {
            event.input.touchedByJs = true;
        });
    }
"#;

/// The handler observes exactly the payload the host dispatched
/// (byte-identical to the literal JSON), its in-place mutation flows back to
/// the caller's `&mut`, and the outcome mirror is `tool_call`-only as
/// documented.
#[tokio::test(flavor = "current_thread")]
async fn the_js_visible_payload_is_byte_identical_and_the_mirror_is_tool_call_only() {
    let temp = TempDir::new("payload-shape");
    let runtime = load_runtime(&temp, &[("echo.ts", JS_ECHO)]).await;
    let host = PluginHost::build(Some(runtime.clone()), Vec::new());

    // A sample event with nested objects/arrays/null in insertion order.
    let literal = r#"{"type":"gate_probe","index":7,"text":"delta","nested":{"a":[1,2],"b":null}}"#;
    let mut payload = json!({"type": "gate_probe", "index": 7, "text": "delta", "nested": {"a": [1, 2], "b": null}});
    let outcome = host
        .emit("gate_probe", &mut payload)
        .await
        .expect("emit probe");
    let seen = runtime
        .eval_json("JSON.stringify(globalThis.__seenPayload)")
        .await
        .expect("read the captured payload");
    assert_eq!(seen, json!(literal), "the JS handler's view of the payload");
    assert_eq!(
        payload["echoed"],
        json!(true),
        "the in-place mutation returns"
    );
    assert!(
        outcome.payload.is_null(),
        "the outcome payload mirror is not populated for non-tool_call events"
    );

    // `tool_call` keeps its documented mirror of the (mutated) payload.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "call-1",
        "toolName": "echo-tool",
        "input": { "k": 1 },
    });
    let outcome = host
        .emit("tool_call", &mut payload)
        .await
        .expect("emit tool_call");
    assert_eq!(payload["input"]["touchedByJs"], json!(true));
    assert_eq!(
        outcome.payload["input"]["touchedByJs"],
        json!(true),
        "tool_call's payload mirror carries the handler rewrites"
    );
}
