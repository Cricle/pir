//! Widget component factories (`ctx.ui.setWidget(key, factory, options)`,
//! upstream types.ts:188-193 overload 2) and the factory-widget TUI
//! consumption path. rpiv-todo's overlay is the reference consumer: it
//! registers once in the aboveEditor slot with a `(tui, theme) => component`
//! factory whose `render(width)` returns the panel lines, then refreshes via
//! `tui.requestRender()`.
//!
//! Locked here:
//! - the JS registration contract (factory marker notification, `requestRender`
//!   repaint signal, theme passed to the factory, factory created once);
//! - the TUI round-trip (`RendererKind::Widget` → `__pir_callRenderer` →
//!   lines) plus registry gating on the exported defs;
//! - the Rust factory-widget registry (refresh-on-dirty, width invalidation,
//!   error isolation to empty lines, placement slots, removal).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::tui::renderers::{
    factory_widget_lines, mark_factory_widgets_dirty, refresh_factory_widgets,
    remove_factory_widget, renderer_registered, upsert_factory_widget, ExtensionRendererInvoker,
    FactoryWidget, RendererInvoker, RendererKind, RendererOutcome, RendererRequest,
    WIDGET_RENDER_REQUEST_KEY,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Real-QuickJS harness (same pattern as tests/renderer_parity.rs)
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-widget-factories-{label}-{}-{}",
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

fn start_handle(dir: &std::path::Path) -> ExtensionHandle {
    ExtensionHandle::start(ExtensionConfig {
        cwd: dir.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui: true,
        model: Some("parity-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start")
}

fn load_extension(handle: &ExtensionHandle, dir: &std::path::Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let path = dir.join(name);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let result = runtime
        .block_on(handle.load(std::slice::from_ref(&path)))
        .expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

fn emit_session_start(handle: &ExtensionHandle) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let mut payload = json!({ "reason": "startup" });
    let outcome = runtime
        .block_on(handle.emit("session_start", &mut payload))
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
}

fn eval_sync(handle: &ExtensionHandle, code: &str) -> Value {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(handle.eval_json(code)).expect("eval")
}

fn invoke_widget(
    invoker: &dyn RendererInvoker,
    key: &str,
    width: usize,
) -> anyhow::Result<RendererOutcome> {
    invoker.invoke(&RendererRequest {
        kind: RendererKind::Widget,
        key: key.to_string(),
        payload: Value::Null,
        options: json!({ "width": width }),
    })
}

/// The LAST widget notification for `key` (later setWidget calls replace
/// earlier ones, mirroring the runtime's last-wins registration).
fn drain_notification(handle: &ExtensionHandle, key: &str) -> Option<Value> {
    let mut found = None;
    for n in handle.drain_notifications() {
        if let pir::extensions::Notification::Widget { id, content } = n {
            if id == key {
                found = Some(content);
            }
        }
    }
    found
}

/// The rpiv-todo overlay registration sequence, verbatim shape: register
/// once with a factory capturing `tui` + `theme`, render per width, refresh
/// via `tui.requestRender()`.
#[test]
fn factory_widget_round_trips_through_the_renderer_bridge() {
    let temp = TempDir::new("round-trip");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "overlay.ts",
        r#"
        export default function (pi) {
          pi.on("session_start", (_event, ctx) => {
            ctx.ui.setWidget("rpiv-todos", (tui, factoryTheme) => {
              globalThis.__factoryCalls = (globalThis.__factoryCalls ?? 0) + 1;
              globalThis.__tuiType = typeof tui.requestRender;
              globalThis.__themeType = typeof factoryTheme.fg;
              globalThis.__ctxThemeType = typeof ctx.ui.theme?.fg;
              globalThis.__toolsExpanded = ctx.ui.getToolsExpanded?.();
              tui.requestRender();
              return {
                render: (width) => [
                  factoryTheme.fg("accent", "Todos (1/2)"),
                  "├─ #1 write tests",
                  "trunc:" + width,
                ],
                invalidate: () => {},
              };
            }, { placement: "aboveEditor" });
          });
        }
        "#,
    );
    emit_session_start(&handle);

    // The registration notifies the TUI with the factory marker (never
    // static text), normalized to the above-transcript slot; the factory's
    // `tui.requestRender()` push rode the reserved status key.
    let content = drain_notification(&handle, "rpiv-todos").expect("factory notification");
    assert_eq!(content["factory"], true, "{content}");
    assert_eq!(content["placement"], "above_transcript", "{content}");
    assert!(
        content.get("text").is_none(),
        "factory widgets carry no static text: {content}"
    );
    // The factory runs lazily on the FIRST render round-trip (component
    // creation), so the repaint signal lands in the queue after the
    // registration drain — assert it on the post-render drain below.

    // Registry export: the TUI registry check sees the widget factory.
    let defs = handle.renderer_defs();
    assert!(
        renderer_registered(&defs, RendererKind::Widget, "rpiv-todos"),
        "widgetRenderers bucket must export the factory: {defs}"
    );

    // The TUI round-trip: the factory's component renders the panel lines.
    let handle = Arc::new(handle);
    let invoker = ExtensionRendererInvoker::new(handle.clone());
    let outcome = invoke_widget(&invoker, "rpiv-todos", 40).expect("widget render");
    // The factory's `tui.requestRender()` (during component creation) rode
    // the reserved status key into the notification queue.
    assert!(
        handle.drain_notifications().into_iter().any(|n| matches!(
            n,
            pir::extensions::Notification::Status { key, .. } if key == WIDGET_RENDER_REQUEST_KEY
        )),
        "expected the requestRender repaint signal"
    );

    // The factory saw a requestRender-capable tui stub, a theme object with
    // fg, and the ctx.ui.theme fallback with fg too.
    assert_eq!(
        eval_sync(
            &handle,
            "JSON.stringify({tui: globalThis.__tuiType, theme: globalThis.__themeType, ctxTheme: globalThis.__ctxThemeType, tools: globalThis.__toolsExpanded})",
        ),
        json!({ "tui": "function", "theme": "function", "ctxTheme": "function", "tools": false }),
        "factory arguments"
    );
    assert_eq!(
        outcome.lines,
        Some(vec![
            "Todos (1/2)".to_string(),
            "├─ #1 write tests".to_string(),
            "trunc:40".to_string(),
        ]),
        "render(width) output"
    );

    // The factory runs ONCE per registration (upstream semantics): a second
    // refresh re-renders the same component without re-running the factory.
    let again = invoke_widget(&invoker, "rpiv-todos", 60).expect("re-render");
    assert_eq!(
        again.lines.unwrap()[2],
        "trunc:60",
        "fresh width per render"
    );
    // eval_json parses the serialized result, so the string "1" comes back
    // as a JSON number.
    let calls = eval_sync(&handle, "JSON.stringify(globalThis.__factoryCalls)");
    assert_eq!(calls, json!(1), "factory created once");
}

/// The theme passed to factories resolves colors from the live host snapshot
/// (hex → truecolor ANSI); unknown tokens pass text through.
#[test]
fn factory_theme_styles_from_the_host_snapshot() {
    let temp = TempDir::new("theme");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "theme.ts",
        r##"
        globalThis.__pir_host.getThemeJson = () => JSON.stringify({
            colors: { accent: "#ff8000", muted: "not-a-color" },
        });
        export default function (pi) {
          pi.on("session_start", (_event, ctx) => {
            ctx.ui.setWidget("styled", (_tui, theme) => ({
              render: () => [
                theme.fg("accent", "styled"),
                theme.fg("muted", "plain"),
                theme.fg("unknown-token", "passthrough"),
                theme.bold("b"),
                theme.strikethrough("s"),
              ],
            }));
          });
        }
        "##,
    );
    emit_session_start(&handle);
    let _ = drain_notification(&handle, "styled");

    let handle = Arc::new(handle);
    let invoker = ExtensionRendererInvoker::new(handle.clone());
    let outcome = invoke_widget(&invoker, "styled", 40).expect("widget render");
    let lines = outcome.lines.expect("lines");
    assert_eq!(
        lines[0], "\u{1b}[38;2;255;128;0mstyled\u{1b}[39m",
        "{lines:?}"
    );
    assert_eq!(lines[1], "plain", "non-hex color passes through: {lines:?}");
    assert_eq!(
        lines[2], "passthrough",
        "unknown token passes through: {lines:?}"
    );
    assert_eq!(lines[3], "\u{1b}[1mb\u{1b}[22m", "{lines:?}");
    assert_eq!(lines[4], "\u{1b}[9ms\u{1b}[29m", "{lines:?}");
}

/// A throwing factory is isolated: the round-trip fails for that widget (the
/// TUI registry degrades it to empty lines) and later registrations recover.
#[test]
fn failing_factory_is_isolated() {
    let temp = TempDir::new("failing");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "failing.ts",
        r#"
        export default function (pi) {
          pi.on("session_start", (_event, ctx) => {
            ctx.ui.setWidget("broken", () => {
              throw new Error("overlay exploded");
            });
          });
        }
        "#,
    );
    emit_session_start(&handle);
    let _ = drain_notification(&handle, "broken");

    let handle = Arc::new(handle);
    let invoker = ExtensionRendererInvoker::new(handle.clone());
    assert!(
        invoke_widget(&invoker, "broken", 40).is_err(),
        "the factory failure surfaces as a round-trip error (the TUI hides the widget)"
    );
}

/// Static re-registration retires the factory (the key falls back to plain
/// text), and `setWidget(key, undefined)` removes it entirely.
#[test]
fn static_and_removal_paths_retire_the_factory() {
    let temp = TempDir::new("retire");
    let handle = start_handle(&temp.0);
    load_extension(
        &handle,
        &temp.0,
        "retire.ts",
        r#"
        export default function (pi) {
          pi.on("session_start", (_event, ctx) => {
            ctx.ui.setWidget("duo", () => ({ render: () => ["factory"] }));
            ctx.ui.setWidget("duo", ["static", "lines"]);
            ctx.ui.setWidget("gone", () => ({ render: () => ["x"] }));
            ctx.ui.setWidget("gone", undefined);
          });
        }
        "#,
    );
    emit_session_start(&handle);
    // One drain carries all four notifications: the factory markers, the
    // static replacement, and the removal — the LAST one per key wins.
    let notifications = handle.drain_notifications();
    let last_widget = |key: &str| -> Option<Value> {
        notifications
            .iter()
            .filter_map(|n| match n {
                pir::extensions::Notification::Widget { id, content } if id == key => {
                    Some(content.clone())
                }
                _ => None,
            })
            .next_back()
    };

    let handle = Arc::new(handle);
    let invoker = ExtensionRendererInvoker::new(handle.clone());
    // Static registration: text notification, factory no longer resolves.
    let content = last_widget("duo").expect("static notification");
    assert_eq!(content["text"], "static\nlines", "{content}");
    assert!(content.get("factory").is_none(), "{content}");
    let outcome = invoke_widget(&invoker, "duo", 40).expect("invoke");
    assert!(
        outcome.is_unhandled(),
        "retired factory must not render: {outcome:?}"
    );

    // Removal: null-content notification, factory gone.
    let content = last_widget("gone").expect("removal notification");
    assert!(content.is_null(), "{content}");
    let outcome = invoke_widget(&invoker, "gone", 40).expect("invoke");
    assert!(outcome.is_unhandled(), "{outcome:?}");
}

// ---------------------------------------------------------------------------
// The Rust factory-widget registry (the TUI-side consumption)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeInvoker {
    /// Per-key scripted outcomes (Ok lines) — the default is `["line"]`.
    lines: Mutex<Vec<(String, Vec<String>)>>,
    calls: AtomicUsize,
    fail: AtomicUsize,
}

impl FakeInvoker {
    fn scripted(&self, key: &str, lines: &[&str]) -> &Self {
        self.lines.lock().unwrap().push((
            key.to_string(),
            lines.iter().map(|l| l.to_string()).collect(),
        ));
        self
    }
}

impl RendererInvoker for FakeInvoker {
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) > 0 {
            self.fail.fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow::anyhow!("factory exploded"));
        }
        let scripted = self.lines.lock().unwrap();
        let lines = scripted
            .iter()
            .rev()
            .find(|(key, _)| *key == request.key)
            .map(|(_, lines)| lines.clone())
            .unwrap_or_else(|| vec!["line".to_string()]);
        Ok(RendererOutcome {
            lines: Some(lines),
            text: None,
        })
    }
}

fn registry() -> Vec<FactoryWidget> {
    Vec::new()
}

#[test]
fn registry_refreshes_dirty_widgets_and_caches_the_lines() {
    let mut registry = registry();
    let invoker = FakeInvoker::default();
    invoker.scripted("rpiv-todos", &["Todos (1/2)", "├─ #1"]);

    upsert_factory_widget(&mut registry, "rpiv-todos", "above_transcript");
    // First refresh renders the dirty widget and reports the change.
    assert!(refresh_factory_widgets(&mut registry, &invoker, 80));
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript"),
        vec![(
            "rpiv-todos".to_string(),
            vec!["Todos (1/2)".to_string(), "├─ #1".to_string()]
        )],
        "cached lines"
    );
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);

    // Clean widget + same width → no mailbox round-trip, no change.
    assert!(!refresh_factory_widgets(&mut registry, &invoker, 80));
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);

    // `tui.requestRender()` (the repaint signal) marks the widget dirty; the
    // next refresh re-renders it.
    mark_factory_widgets_dirty(&mut registry);
    invoker.scripted("rpiv-todos", &["Todos (2/2)"]);
    assert!(refresh_factory_widgets(&mut registry, &invoker, 80));
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        vec!["Todos (2/2)".to_string()],
        "refreshed content"
    );
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 2);

    // A width change (resize) re-renders even clean widgets; the new
    // content marks the frame dirty.
    invoker.scripted("rpiv-todos", &["Todos (2/2) @120"]);
    assert!(refresh_factory_widgets(&mut registry, &invoker, 120));
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        vec!["Todos (2/2) @120".to_string()],
    );
    // A resize that renders the SAME lines is a re-render without a
    // content change (no repaint needed).
    assert!(!refresh_factory_widgets(&mut registry, &invoker, 130));
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 4);
}

#[test]
fn registry_isolates_failing_widgets_to_empty_lines() {
    let mut registry = registry();
    let invoker = FakeInvoker::default();
    upsert_factory_widget(&mut registry, "broken", "above_transcript");
    assert!(refresh_factory_widgets(&mut registry, &invoker, 80));
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        vec!["line".to_string()],
        "the default scripted lines are cached on success"
    );

    // A failing round-trip hides the widget's lines but keeps it registered.
    invoker.fail.store(1, Ordering::SeqCst);
    mark_factory_widgets_dirty(&mut registry);
    let _ = refresh_factory_widgets(&mut registry, &invoker, 80);
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        Vec::<String>::new(),
        "failure degrades to empty lines"
    );
    // The registry still tracks the widget (it can recover on the next
    // successful render).
    assert_eq!(factory_widget_lines(&registry, "above_transcript").len(), 1);
}

#[test]
fn registry_placement_slots_and_removal() {
    let mut registry = registry();
    let invoker = FakeInvoker::default();
    upsert_factory_widget(&mut registry, "above", "above_transcript");
    upsert_factory_widget(&mut registry, "below", "below_transcript");
    assert!(refresh_factory_widgets(&mut registry, &invoker, 80));

    // Slots filter by placement.
    let above = factory_widget_lines(&registry, "above_transcript");
    let below = factory_widget_lines(&registry, "below_transcript");
    assert_eq!(
        above.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec!["above"]
    );
    assert_eq!(
        below.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec!["below"]
    );

    // Re-placement moves the widget between slots and re-renders it.
    upsert_factory_widget(&mut registry, "above", "below_transcript");
    assert!(refresh_factory_widgets(&mut registry, &invoker, 80));
    let above = factory_widget_lines(&registry, "above_transcript");
    let below = factory_widget_lines(&registry, "below_transcript");
    assert!(above.is_empty(), "moved out of the above slot: {above:?}");
    assert_eq!(below.len(), 2, "{below:?}");

    // Removal drops the widget (setWidget(key, undefined)).
    remove_factory_widget(&mut registry, "below");
    remove_factory_widget(&mut registry, "above");
    assert!(factory_widget_lines(&registry, "below_transcript").is_empty());
    assert!(factory_widget_lines(&registry, "above_transcript").is_empty());
}
