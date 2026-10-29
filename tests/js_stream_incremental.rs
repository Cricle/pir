//! Incremental streaming for the JS-callback `streamSimple` provider
//! (upstream's live `AssistantMessageEventStream`, produced event-by-event
//! across the QuickJS boundary).
//!
//! Covered here (independent binary — suites/extensions_js_stream.rs is the
//! batch-era coverage):
//!
//! 1. Incremental async-iterator callbacks stream events one at a time: the
//!    first event arrives while the JS generator is still parked (the test
//!    only then opens the gate it waits on), in order.
//! 2. Legacy batch forms stay compatible: a plain array and an
//!    `{events: [...]}` wrapper replay in order through the same channel.
//! 3. Midstream abort terminates the provider stream with the typed aborted
//!    error, closes the JS session (generator `finally` runs), and leaves the
//!    runtime usable.
//! 4. Fail-closed: a generator that throws midstream and one that yields an
//!    invalid event both terminate with a typed error event.
//! 5. streamSimple options hooks (upstream `onPayload` / `onResponse` /
//!    `onProviderStreamEvent`) fire through the JS bridge, isolated.
//! 6. Virtual-model route sticky state: the bridge feeds the last returned
//!    `route.state` back as `request.state` (upstream `ModelRoute.state`).
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use pir::ai::provider::Provider;
use pir::ai::providers::js_stream::{js_virtual_route_callback, JsStreamProvider};
use pir::ai::types::*;
use pir::ai::virtual_::{VirtualRouteRequest, VIRTUAL_MODEL_API};
use pir::extensions::runtime::HookStreamItem;
use pir::extensions::{ExtensionConfig, ExtensionHandle};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn js_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Per-test temp directory removed at the end of each test body.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-js-stream-incremental-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn load_extension(handle: &ExtensionHandle, dir: &std::path::Path, source: &str) {
    let path = dir.join("ext.ts");
    std::fs::write(&path, source).unwrap();
    let result = handle.load(std::slice::from_ref(&path)).await.unwrap();
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

fn js_model(id: &str, provider: &str) -> Model {
    Model {
        id: id.to_string(),
        name: "JS model".to_string(),
        api: "openai-chat".to_string(),
        provider: provider.to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// Wire request handed to the JS hook through the streaming channel.
fn stream_payload() -> serde_json::Value {
    json!({
        "model": {
            "id": "m", "name": "M", "api": "openai-chat",
            "provider": "prov", "baseUrl": "http://localhost",
        },
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
        "options": {},
    })
}

const EXTENSION_TS: &str = r#"
export default function (pi) {
  const usage = {
    input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
  };
  const delta = (index, text) => ({ type: "text_delta", content_index: index, delta: text });
  const doneEvent = (model, text) => ({
    type: "done", reason: "stop",
    message: {
      role: "assistant",
      content: [{ type: "text", text }],
      api: model.api ?? "openai-chat", provider: model.provider ?? "prov", model: model.id ?? "m",
      usage, stopReason: "stop", timestamp: 1,
    },
  });
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const register = (id, streamSimple) => pi.registerProvider(id, {
    baseUrl: "http://localhost",
    api: "openai-chat",
    models: [{ id: `${id}-model`, name: `${id} model`, contextWindow: 1000, maxTokens: 100, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    streamSimple,
  });

  // 1. Incremental: one delta, then parks on a gate the host opens via
  //    eval_json — proving the first event streams before the callback ends.
  let gateResolve = null;
  const gate = new Promise((resolve) => { gateResolve = resolve; });
  globalThis.__pir_openGate = () => { gateResolve(); return JSON.stringify({ opened: true }); };
  register("incprov", async function* (model) {
    yield delta(0, "chunk-1");
    await gate;
    yield delta(0, "-chunk-2");
    yield doneEvent(model, "chunk-1-chunk-2");
  });

  // 2a. Legacy batch array form.
  register("batchprov", (model) => [
    delta(0, "batch-a"), delta(0, "-batch-b"), doneEvent(model, "batch-a-batch-b"),
  ]);

  // 2b. Legacy {events: [...]} wrapper form.
  register("wrappedprov", (model) => ({
    events: [delta(0, "wrapped"), doneEvent(model, "wrapped")],
  }));

  // 3. Aborting: a slow ticker whose finally marks the JS-side close.
  register("parkprov", async function* () {
    yield delta(0, "before-park");
    try {
      await sleep(40); yield delta(0, "tick-1");
      await sleep(40); yield delta(0, "tick-2");
    } finally {
      globalThis.__pir_parkprovCancelled = true;
    }
  });

  // 4a. Midstream throw after one streamed event.
  register("boomprov", async function* () {
    yield delta(0, "partial");
    throw new Error("midstream boom");
  });

  // 4b. Invalid event shape.
  register("badprov", async function* () {
    yield { notAnEvent: true };
  });

  // 5. Options hooks: the callback requires and fires the three wrappers.
  register("hookprov", async function* (model, context, options) {
    if (typeof options.onPayload !== "function") throw new Error("onPayload hook missing");
    if (typeof options.onResponse !== "function") throw new Error("onResponse hook missing");
    if (typeof options.onProviderStreamEvent !== "function") throw new Error("onProviderStreamEvent hook missing");
    options.onPayload({ marker: "request-body" });
    options.onResponse(200, { "x-test": "yes" });
    yield delta(0, "hooked");
    options.onProviderStreamEvent({ raw: "chunk" });
    yield doneEvent(model, "hooked");
  });

  // 6. Virtual-model route with sticky state: bumps a counter stored in
  //    request.state across calls (undefined on the first call).
  pi.registerVirtualModel({
    id: "sticky",
    physical: { provider: "fake", id: "fake-model" },
    route: (request) => ({
      model: { provider: "fake", id: "fake-model" },
      state: { n: ((request.state && request.state.n) ?? 0) + 1 },
    }),
  });
}
"#;

// ---------------------------------------------------------------------------
// 1. Incremental: events stream while the generator is still parked
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn incremental_events_stream_before_the_generator_finishes() {
    let dir = temp_dir("incremental");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    load_extension(&handle, &dir, EXTENSION_TS).await;

    let mut rx = handle
        .call_hook_stream("__pir_stream:incprov", &stream_payload())
        .await;

    // First delta arrives while the generator is parked on the (still
    // closed) gate; if the channel were batch-only this recv would time out.
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("first event must arrive while the generator is parked");
    match first {
        Some(HookStreamItem::Event(event)) => assert_eq!(
            event.get("delta").and_then(serde_json::Value::as_str),
            Some("chunk-1"),
            "first streamed event is the first generator chunk: {event}"
        ),
        other => panic!("expected an event item, got {other:?}"),
    }

    // Only now let the generator finish; the remaining events stream through.
    handle
        .eval_json("globalThis.__pir_openGate()")
        .await
        .unwrap();

    let mut deltas = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("stream must finish after the gate opens")
        {
            Some(HookStreamItem::Event(event)) => {
                if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str) {
                    deltas.push(delta.to_owned());
                }
            }
            Some(HookStreamItem::End) => break,
            Some(HookStreamItem::Failed(message)) => panic!("stream failed: {message}"),
            None => panic!("channel closed before End"),
        }
    }
    assert_eq!(
        deltas,
        vec!["-chunk-2".to_string()],
        "remaining deltas stream in order (chunk-1 was already consumed)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. Legacy batch forms stay compatible through the same channel
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn legacy_batch_forms_stream_through_the_same_channel() {
    let dir = temp_dir("batch");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    load_extension(&handle, &dir, EXTENSION_TS).await;

    for (key, expected_deltas) in [
        ("__pir_stream:batchprov", vec!["batch-a", "-batch-b"]),
        ("__pir_stream:wrappedprov", vec!["wrapped"]),
    ] {
        let mut rx = handle.call_hook_stream(key, &stream_payload()).await;
        let mut deltas = Vec::new();
        let mut saw_done = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{key}: stream must finish"))
            {
                Some(HookStreamItem::Event(event)) => {
                    if event.get("type").and_then(serde_json::Value::as_str) == Some("done") {
                        saw_done = true;
                    }
                    if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str) {
                        deltas.push(delta.to_owned());
                    }
                }
                Some(HookStreamItem::End) => break,
                Some(HookStreamItem::Failed(message)) => panic!("{key}: {message}"),
                None => panic!("{key}: channel closed before End"),
            }
        }
        assert_eq!(
            deltas, expected_deltas,
            "{key}: batch deltas replay in order"
        );
        assert!(saw_done, "{key}: the done event reaches the consumer");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. Midstream abort: typed aborted error + JS session close + usable runtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn abort_midstream_terminates_typed_closes_the_js_session_and_keeps_runtime_usable() {
    let dir = temp_dir("abort");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    load_extension(&handle, &dir, EXTENSION_TS).await;

    let provider = JsStreamProvider::new(
        "parkprov",
        vec![js_model("parkprov-model", "parkprov")],
        handle.clone(),
    );
    let signal = AbortSignal::new();
    let stream = provider.stream(
        &js_model("parkprov-model", "parkprov"),
        Context::default(),
        StreamOptions {
            signal: Some(signal.clone()),
            ..Default::default()
        },
    );

    // Wait until the first event streamed through, then abort mid-stream.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_first = false;
    while std::time::Instant::now() < deadline {
        match stream.try_next() {
            Some(AssistantMessageEvent::TextDelta { delta, .. }) if delta == "before-park" => {
                saw_first = true;
                break;
            }
            Some(_) => continue,
            // Empty queue: yield to the producer task and poll again.
            None => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    assert!(
        saw_first,
        "the first incremental event must arrive before abort"
    );
    signal.abort();

    let terminal = stream
        .result()
        .await
        .expect("abort must terminate the stream");
    // pir convention (matching the pre-existing start-abort path in
    // JsStreamProvider::stream): the terminal is an Error event with
    // ErrorReason::Aborted; the message names the abort and the agent loop
    // maps the signal itself.
    assert_eq!(terminal.stop_reason, StopReason::Error);
    let error = terminal.error_message.as_deref().unwrap_or_default();
    assert!(error.contains("aborted"), "typed aborted terminal: {error}");

    // The runtime is still usable after the abort.
    let alive = handle
        .eval_json("JSON.stringify({ alive: true })")
        .await
        .expect("runtime stays usable after abort");
    assert_eq!(alive.get("alive"), Some(&json!(true)));

    // The JS session was closed through __pir_streamCancel: the generator's
    // finally block ran (async-generator return()).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut cancelled = false;
    while std::time::Instant::now() < deadline {
        let probe = handle
            .eval_json("JSON.stringify({ cancelled: globalThis.__pir_parkprovCancelled === true })")
            .await
            .unwrap();
        if probe.get("cancelled") == Some(&json!(true)) {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        cancelled,
        "the parked JS generator's finally block must run"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. Fail-closed: midstream throw and invalid event shapes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn midstream_failures_terminate_with_typed_errors() {
    let dir = temp_dir("failclosed");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    load_extension(&handle, &dir, EXTENSION_TS).await;

    // 4a. Generator throws after one streamed event.
    let provider = JsStreamProvider::new(
        "boomprov",
        vec![js_model("boomprov-model", "boomprov")],
        handle.clone(),
    );
    let stream = provider.stream(
        &js_model("boomprov-model", "boomprov"),
        Context::default(),
        StreamOptions::default(),
    );
    let terminal = stream
        .result()
        .await
        .expect("throw must terminate the stream");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    let error = terminal.error_message.as_deref().unwrap_or_default();
    assert!(
        error.contains("midstream boom"),
        "typed error carries the JS error: {error}"
    );

    // 4b. Generator yields an invalid event shape.
    let provider = JsStreamProvider::new(
        "badprov",
        vec![js_model("badprov-model", "badprov")],
        handle.clone(),
    );
    let stream = provider.stream(
        &js_model("badprov-model", "badprov"),
        Context::default(),
        StreamOptions::default(),
    );
    let terminal = stream
        .result()
        .await
        .expect("invalid event must terminate the stream");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    let error = terminal.error_message.as_deref().unwrap_or_default();
    assert!(
        error.contains("invalid event"),
        "typed error names the invalid event: {error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 5. streamSimple options hooks fire through the JS bridge
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn stream_simple_options_hooks_fire_through_the_bridge() {
    let dir = temp_dir("hooks");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    load_extension(&handle, &dir, EXTENSION_TS).await;

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let payload_log = Arc::clone(&events);
    let response_log = Arc::clone(&events);
    let event_log = Arc::clone(&events);

    let provider = JsStreamProvider::new(
        "hookprov",
        vec![js_model("hookprov-model", "hookprov")],
        handle.clone(),
    );
    let hooks = ProviderStreamHooks {
        on_payload: Some(Arc::new(move |payload| {
            payload_log.lock().unwrap().push(format!(
                "payload:{}",
                payload
                    .get("marker")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            ));
        })),
        on_response: Some(Arc::new(move |status, headers| {
            response_log.lock().unwrap().push(format!(
                "response:{status}:{}",
                headers.get("x-test").map(String::as_str).unwrap_or("?")
            ));
        })),
        on_provider_stream_event: Some(Arc::new(move |event| {
            event_log.lock().unwrap().push(format!(
                "event:{}",
                event
                    .get("raw")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            ));
        })),
        ..Default::default()
    };

    let stream = provider.stream(
        &js_model("hookprov-model", "hookprov"),
        Context::default(),
        StreamOptions {
            hooks,
            ..Default::default()
        },
    );
    let terminal = stream.result().await.expect("hooked stream must terminate");
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "payload:request-body".to_string(),
            "response:200:yes".to_string(),
            "event:chunk".to_string(),
        ],
        "the three options hooks fired through the JS bridge, in order"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 6. Virtual-model route sticky state
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn virtual_route_state_is_sticky_across_calls() {
    let dir = temp_dir("sticky");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    load_extension(&handle, &dir, EXTENSION_TS).await;

    let callback =
        js_virtual_route_callback(handle.clone(), "fake".to_string(), "sticky".to_string());
    let request = VirtualRouteRequest {
        model: json!({ "id": "sticky", "provider": "fake", "api": VIRTUAL_MODEL_API }),
        thinking_level: Some("off".to_string()),
        messages: json!([]),
    };

    let first = callback(request.clone()).await.expect("first route call");
    assert_eq!(
        first.state,
        Some(json!({ "n": 1 })),
        "undefined request.state counts as 0"
    );

    let second = callback(request).await.expect("second route call");
    assert_eq!(
        second.state,
        Some(json!({ "n": 2 })),
        "the previous decision state was fed back as request.state"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
