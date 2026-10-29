//! Native widget/footer/header factory parity (`ctx.ui.setWidget(key,
//! factory, placement)`, `setFooter(factory)`, `setHeader(factory)`).
//!
//! A native plugin assigns factories through [`UiService`]; the TUI renders
//! them through the SAME `FactoryWidget` caches, registration notifications
//! and refresh helpers the JS `__pir_callRenderer` bridge uses. The tests
//! lock the contract:
//!
//! - a native widget registers at the requested dock placement, renders at
//!   the frame width and refreshes on registration / invalidation / resize;
//! - native footer/header factories replace the built-in bars; clearing
//!   restores the built-ins;
//! - native factories resolve BEFORE a JS registration for the same slot
//!   (native wins), and native removal reveals the JS factory instead of
//!   clearing it;
//! - a panicking native factory is isolated and falls back to the JS face,
//!   then to empty lines;
//! - native render callbacks run under the `RendererUiGuard`, so an async
//!   `ctx.ui` dialog requested by a factory resolves immediately;
//! - no native registrations = the JS-only behavior is unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use serde_json::{json, Value as JsonValue};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::plugins::plugin::services::{
    BarFactory, DialogOptions, FactoryRenderContext, RendererKind as ServiceRendererKind,
    RendererService, ThemeSnapshot, UiService, WidgetFactory, WidgetPlacement,
};
use pir::plugins::plugin::{AgentPlugin, PluginContext};
use pir::plugins::PluginInfo;
use pir::tui::renderers::{
    factory_widget_lines, mark_factory_widgets_dirty, refresh_factory_bars,
    refresh_factory_widgets, set_factory_slot, upsert_factory_widget, FactoryRendererInvoker,
    FactoryWidget, RendererInvoker, RendererKind, RendererOutcome, RendererRequest,
    WIDGET_RENDER_REQUEST_KEY,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-plugin-widget-factories-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Captures the `PluginContext` the live session binds.
struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
}

impl ProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the session bound a plugin context")
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

fn fake_model() -> Model {
    Model {
        id: "model-a".to_owned(),
        name: "Model A".to_owned(),
        provider: "fake".to_owned(),
        api: "fake".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
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

/// Build a hermetic session with one inline plugin and an optional extension
/// handle (the presentation sink the native `UiService` rides).
fn build_session(
    plugin: Arc<ProbePlugin>,
    extensions: Option<ExtensionHandle>,
    cwd: &Path,
) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: None,
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions,
            tool_selection: None,
        },
        vec![plugin as Arc<dyn AgentPlugin>],
    )
}

/// Plan-only handle: the shared presentation queue and renderer UI guard,
/// without constructing QuickJS.
fn plan_only_handle(cwd: &Path) -> ExtensionHandle {
    ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_owned(),
        has_ui: true,
        model: Some("factory-model".to_owned()),
        session_id: Some("native-factory-test".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
}

struct Fixture {
    #[allow(dead_code)]
    temp: TempDir,
    handle: ExtensionHandle,
    plugin: Arc<ProbePlugin>,
    #[allow(dead_code)]
    session: Arc<AgentSession>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let temp = TempDir::new(label);
        let handle = plan_only_handle(temp.path());
        let plugin = ProbePlugin::new("native-factory-probe");
        let session = build_session(Arc::clone(&plugin), Some(handle.clone()), temp.path());
        Self {
            temp,
            handle,
            plugin,
            session,
        }
    }

    fn ui(&self) -> Arc<dyn UiService> {
        self.plugin
            .last_context()
            .ui_arc()
            .expect("ui service is bound with a presentation sink")
    }

    fn renderers(&self) -> Arc<dyn RendererService> {
        self.plugin
            .last_context()
            .renderers_arc()
            .expect("renderer service is bound")
    }

    fn notifications(&self) -> Vec<Notification> {
        self.handle.take_notifications(|_| true)
    }
}

fn test_theme() -> ThemeSnapshot {
    ThemeSnapshot {
        name: Arc::from("dark"),
        data: Arc::new(json!({ "colors": { "accent": "#ff8000" } })),
    }
}

/// Scripted JS-side renderer invoker: one outcome per key, plus a call count.
#[derive(Default)]
struct FakeJsInvoker {
    calls: AtomicUsize,
    lines: Mutex<HashMap<String, Vec<String>>>,
}

impl FakeJsInvoker {
    fn scripted(&self, key: &str, lines: &[&str]) {
        self.lines.lock().unwrap().insert(
            key.to_owned(),
            lines.iter().map(|line| (*line).to_owned()).collect(),
        );
    }
}

impl RendererInvoker for FakeJsInvoker {
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let lines = self
            .lines
            .lock()
            .unwrap()
            .get(&request.key)
            .cloned()
            .unwrap_or_else(|| vec![format!("js:{}", request.key)]);
        Ok(RendererOutcome {
            lines: Some(lines),
            text: None,
        })
    }
}

/// A native widget factory that reports the render context it observed.
struct ProbeWidget {
    prefix: &'static str,
    calls: AtomicUsize,
    last_width: AtomicUsize,
    saw_theme: AtomicBool,
    request_render_calls: AtomicUsize,
}

impl ProbeWidget {
    fn new(prefix: &'static str) -> Arc<Self> {
        Arc::new(Self {
            prefix,
            calls: AtomicUsize::new(0),
            last_width: AtomicUsize::new(0),
            saw_theme: AtomicBool::new(false),
            request_render_calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl WidgetFactory for ProbeWidget {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.last_width.store(context.width, Ordering::SeqCst);
        self.saw_theme
            .store(context.theme.is_some(), Ordering::SeqCst);
        if let Some(request_render) = context.request_render {
            request_render();
            self.request_render_calls.fetch_add(1, Ordering::SeqCst);
        }
        vec![format!("{}:{}:{}", self.prefix, context.width, call)]
    }
}

/// A native bar factory that records the footer data snapshot and width.
struct ProbeBar {
    prefix: &'static str,
    calls: AtomicUsize,
    last_width: AtomicUsize,
    footer_branch: Mutex<Option<String>>,
}

impl ProbeBar {
    fn new(prefix: &'static str) -> Arc<Self> {
        Arc::new(Self {
            prefix,
            calls: AtomicUsize::new(0),
            last_width: AtomicUsize::new(0),
            footer_branch: Mutex::new(None),
        })
    }
}

impl BarFactory for ProbeBar {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_width.store(context.width, Ordering::SeqCst);
        if let Some(data) = context.footer_data {
            *self.footer_branch.lock().unwrap() = data
                .get("gitBranch")
                .and_then(JsonValue::as_str)
                .map(str::to_owned);
        }
        vec![format!("{}:{}", self.prefix, context.width)]
    }
}

struct PanicWidget;

impl WidgetFactory for PanicWidget {
    fn render(&self, _context: &FactoryRenderContext<'_>) -> Vec<String> {
        panic!("native widget factory boom")
    }
}

struct PanicBar;

impl BarFactory for PanicBar {
    fn render(&self, _context: &FactoryRenderContext<'_>) -> Vec<String> {
        panic!("native bar factory boom")
    }
}

/// A widget factory that asks the native `UiService` for a dialog while it
/// renders: the renderer UI guard must make it resolve immediately.
struct GuardProbeWidget {
    handle: ExtensionHandle,
    ui: Arc<dyn UiService>,
    observed_has_ui: AtomicBool,
    dialog_answer: Mutex<Option<String>>,
}

impl WidgetFactory for GuardProbeWidget {
    fn render(&self, _context: &FactoryRenderContext<'_>) -> Vec<String> {
        self.observed_has_ui
            .store(self.handle.config().has_ui, Ordering::SeqCst);
        let answer = self
            .ui
            .select("guard", &[], DialogOptions::default())
            .now_or_never();
        *self.dialog_answer.lock().unwrap() = Some(format!("{answer:?}"));
        vec!["guarded".to_owned()]
    }
}

fn widget_request(key: &str, width: usize) -> RendererRequest {
    RendererRequest {
        kind: RendererKind::Widget,
        key: key.to_owned(),
        payload: JsonValue::Null,
        options: json!({ "width": width }),
    }
}

fn bar_request(kind: RendererKind, key: &str, width: usize, extra: JsonValue) -> RendererRequest {
    let mut options = match extra {
        JsonValue::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    options.insert("width".to_owned(), JsonValue::from(width));
    RendererRequest {
        kind,
        key: key.to_owned(),
        payload: JsonValue::Null,
        options: JsonValue::Object(options),
    }
}

// ---------------------------------------------------------------------------
// Native widget rendering / refresh
// ---------------------------------------------------------------------------

#[test]
fn native_widget_renders_at_placement_and_refreshes_on_invalidation() {
    let fixture = Fixture::new("widget");
    let ui = fixture.ui();
    let renderers = fixture.renderers();
    let factory = ProbeWidget::new("native");

    ui.set_widget("native-w", factory.clone(), WidgetPlacement::BelowEditor);

    // The registration rides the SAME notification the JS prelude emits, so
    // the TUI's one FactoryWidget cache learns the key and its dock slot.
    let notifications = fixture.notifications();
    assert!(
        notifications.iter().any(|notification| matches!(
            notification,
            Notification::Widget { id, content }
                if id == "native-w"
                    && content.get("factory").and_then(JsonValue::as_bool) == Some(true)
                    && content.get("placement").and_then(JsonValue::as_str)
                        == Some("below_transcript")
        )),
        "factory registration notification: {notifications:?}"
    );
    assert!(renderers.has(ServiceRendererKind::Widget, "native-w"));
    assert!(
        renderers.has_native_factories(),
        "the TUI's JS-only fast path must observe the native slot"
    );
    assert_eq!(
        renderers.widget_placement("native-w"),
        Some(WidgetPlacement::BelowEditor)
    );

    // The TUI drain's cache update, verbatim.
    let mut registry = Vec::new();
    upsert_factory_widget(&mut registry, "native-w", "below_transcript");

    let js = FakeJsInvoker::default();
    let theme = || Some(test_theme());
    // The TUI's `factory_request_render(app)` hook: queue the reserved repaint
    // key so the next drain refreshes every factory slot.
    let request_render = {
        let handle = fixture.handle.clone();
        move || {
            handle.push_notification(Notification::Status {
                key: WIDGET_RENDER_REQUEST_KEY.to_owned(),
                text: None,
            });
        }
    };
    let invoker = FactoryRendererInvoker::new(Some(renderers.as_ref()), Some(&js))
        .with_guard(Some(&fixture.handle))
        .with_theme(Some(&theme))
        .with_request_render(Some(&request_render));

    // First tick renders the dirty widget through the native factory, at the
    // frame width, with a theme snapshot.
    assert!(refresh_factory_widgets(&mut registry, &invoker, 40));
    assert_eq!(
        factory_widget_lines(&registry, "below_transcript")[0].1,
        vec!["native:40:1".to_owned()]
    );
    assert!(
        factory_widget_lines(&registry, "above_transcript").is_empty(),
        "requested below-editor placement"
    );
    assert_eq!(factory.last_width.load(Ordering::SeqCst), 40);
    assert!(
        factory.saw_theme.load(Ordering::SeqCst),
        "the native factory receives the live theme snapshot"
    );
    assert_eq!(js.calls.load(Ordering::SeqCst), 0, "native wins the slot");

    // A clean tick answers from the cache: no factory call, no change.
    assert!(!refresh_factory_widgets(&mut registry, &invoker, 40));
    assert_eq!(factory.calls(), 1);

    // `tui.requestRender()` / theme swap / any invalidation marks the widget
    // dirty; the next tick re-renders it through the same cache path.
    mark_factory_widgets_dirty(&mut registry);
    assert!(refresh_factory_widgets(&mut registry, &invoker, 40));
    assert_eq!(
        factory_widget_lines(&registry, "below_transcript")[0].1,
        vec!["native:40:2".to_owned()]
    );
    assert_eq!(factory.calls(), 2);

    // A resize re-renders even a clean widget at the new width.
    assert!(refresh_factory_widgets(&mut registry, &invoker, 60));
    assert_eq!(
        factory_widget_lines(&registry, "below_transcript")[0].1,
        vec!["native:60:3".to_owned()]
    );
    assert_eq!(factory.last_width.load(Ordering::SeqCst), 60);

    // The `request_render` hook queues the reserved repaint key exactly like
    // the JS `tui.requestRender()` stub.
    assert!(factory.request_render_calls.load(Ordering::SeqCst) > 0);
    let notifications = fixture.notifications();
    assert!(
        notifications.iter().any(|notification| matches!(
            notification,
            Notification::Status { key, .. } if key == WIDGET_RENDER_REQUEST_KEY
        )),
        "request_render must queue the reserved repaint key: {notifications:?}"
    );
}

// ---------------------------------------------------------------------------
// Footer / header
// ---------------------------------------------------------------------------

#[test]
fn native_footer_and_header_replace_builtins_and_clearing_restores_them() {
    let fixture = Fixture::new("bars");
    let ui = fixture.ui();
    let renderers = fixture.renderers();
    let footer = ProbeBar::new("native-footer");
    let header = ProbeBar::new("native-header");

    ui.set_footer(Some(footer.clone()));
    ui.set_header(Some(header.clone()));
    let notifications = fixture.notifications();
    assert!(
        notifications
            .iter()
            .any(|notification| matches!(notification, Notification::Footer(true))),
        "footer registration: {notifications:?}"
    );
    assert!(
        notifications
            .iter()
            .any(|notification| matches!(notification, Notification::Header(true))),
        "header registration: {notifications:?}"
    );
    assert!(renderers.has(ServiceRendererKind::Footer, "__footer"));
    assert!(renderers.has(ServiceRendererKind::Header, "__header"));

    // The TUI drain's slot registration, verbatim.
    let mut footer_slot: Option<FactoryWidget> = None;
    let mut header_slot: Option<FactoryWidget> = None;
    set_factory_slot(&mut footer_slot, "__footer", true);
    set_factory_slot(&mut header_slot, "__header", true);

    let js = FakeJsInvoker::default();
    let theme = || Some(test_theme());
    let invoker = FactoryRendererInvoker::new(Some(renderers.as_ref()), Some(&js))
        .with_guard(Some(&fixture.handle))
        .with_theme(Some(&theme));

    // Both slots render through the native factories; the footer receives
    // the live footerData snapshot.
    assert!(refresh_factory_bars(
        &mut footer_slot,
        &mut header_slot,
        &invoker,
        72,
        || json!({ "footerData": { "gitBranch": "main" } })
    ));
    assert_eq!(
        footer_slot.as_ref().expect("custom footer").lines,
        vec!["native-footer:72".to_owned()]
    );
    assert_eq!(
        header_slot.as_ref().expect("custom header").lines,
        vec!["native-header:72".to_owned()]
    );
    assert_eq!(
        *footer.footer_branch.lock().unwrap(),
        Some("main".to_owned()),
        "the native footer factory receives footerData"
    );
    assert_eq!(footer.last_width.load(Ordering::SeqCst), 72);
    assert_eq!(js.calls.load(Ordering::SeqCst), 0);

    // Clean tick: cached lines, no factory call.
    assert!(!refresh_factory_bars(
        &mut footer_slot,
        &mut header_slot,
        &invoker,
        72,
        || json!({ "footerData": { "gitBranch": "main" } })
    ));
    assert_eq!(footer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(header.calls.load(Ordering::SeqCst), 1);

    // Clearing the native footer restores the built-in: no JS footer is
    // registered, so the slot is removed.
    ui.set_footer(None);
    assert!(
        fixture
            .notifications()
            .iter()
            .any(|notification| matches!(notification, Notification::Footer(false))),
        "native footer removal without a JS footer clears the slot"
    );
    set_factory_slot(&mut footer_slot, "__footer", false);
    assert!(footer_slot.is_none(), "built-in footer restored");

    ui.set_header(None);
    assert!(fixture
        .notifications()
        .iter()
        .any(|notification| matches!(notification, Notification::Header(false))));
    set_factory_slot(&mut header_slot, "__header", false);
    assert!(header_slot.is_none(), "built-in header restored");

    // A second clear is a native no-op: it must not emit another removal
    // notification (and in particular must never clear a JS registration).
    ui.set_footer(None);
    assert!(
        !fixture
            .notifications()
            .iter()
            .any(|notification| matches!(notification, Notification::Footer(_))),
        "clearing a slot the native face does not own is a no-op"
    );
}

// ---------------------------------------------------------------------------
// Panic isolation / fallback
// ---------------------------------------------------------------------------

#[test]
fn panicking_native_factory_falls_back_to_js_then_empty_lines() {
    let fixture = Fixture::new("panic");
    let ui = fixture.ui();
    let renderers = fixture.renderers();

    ui.set_widget(
        "boom-w",
        Arc::new(PanicWidget),
        WidgetPlacement::AboveEditor,
    );
    let _ = fixture.notifications();
    ui.set_footer(Some(Arc::new(PanicBar)));
    let _ = fixture.notifications();

    let js = FakeJsInvoker::default();
    js.scripted("boom-w", &["js-widget"]);
    js.scripted("__footer", &["js-footer"]);
    let theme = || Some(test_theme());
    let invoker = FactoryRendererInvoker::new(Some(renderers.as_ref()), Some(&js))
        .with_guard(Some(&fixture.handle))
        .with_theme(Some(&theme));

    // Registered but panicking: the service isolates the unwind and reports
    // "not handled", so the composite falls through to the JS face.
    let widget = invoker.invoke(&widget_request("boom-w", 40)).unwrap();
    assert_eq!(
        widget.lines,
        Some(vec!["js-widget".to_owned()]),
        "panic falls back to the JS factory"
    );
    let footer = invoker
        .invoke(&bar_request(
            RendererKind::Footer,
            "__footer",
            80,
            json!({ "footerData": {} }),
        ))
        .unwrap();
    assert_eq!(
        footer.lines,
        Some(vec!["js-footer".to_owned()]),
        "panicking bar falls back to the JS factory"
    );

    // Cache the JS fallback through the real refresh path, then drop the JS
    // face: the panic degrades to empty lines (the same isolation a failing
    // JS factory gets); the TUI frame survives.
    let mut registry = Vec::new();
    upsert_factory_widget(&mut registry, "boom-w", "above_transcript");
    assert!(refresh_factory_widgets(&mut registry, &invoker, 40));
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        vec!["js-widget".to_owned()]
    );
    let no_js = FactoryRendererInvoker::new(Some(renderers.as_ref()), None);
    mark_factory_widgets_dirty(&mut registry);
    assert!(refresh_factory_widgets(&mut registry, &no_js, 40));
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        Vec::<String>::new()
    );
    assert!(
        no_js
            .invoke(&bar_request(
                RendererKind::Footer,
                "__footer",
                80,
                json!({})
            ))
            .is_ok(),
        "a panicking factory never unwinds through the invoker"
    );
}

// ---------------------------------------------------------------------------
// JS + native coexistence (real QuickJS bridge)
// ---------------------------------------------------------------------------

fn start_handle(dir: &Path) -> ExtensionHandle {
    ExtensionHandle::start(ExtensionConfig {
        cwd: dir.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui: true,
        model: Some("coexist-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start")
}

fn load_extension(handle: &ExtensionHandle, dir: &Path, name: &str, source: &str) {
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

#[test]
fn native_factories_win_at_a_slot_and_removal_reveals_the_js_factory() {
    let temp = TempDir::new("coexist");
    let handle = start_handle(temp.path());
    load_extension(
        &handle,
        temp.path(),
        "coexist.ts",
        r#"
        export default function (pi) {
          pi.on("session_start", (_event, ctx) => {
            ctx.ui.setWidget("coexist", () => ({
              render: (width) => ["js-widget:" + width],
            }));
            ctx.ui.setFooter(() => ({
              render: (width) => ["js-footer:" + width],
            }));
          });
        }
        "#,
    );
    let plugin = ProbePlugin::new("coexist-probe");
    let session = build_session(Arc::clone(&plugin), Some(handle.clone()), temp.path());
    emit_session_start(&handle);

    let ctx = plugin.last_context();
    let ui = ctx.ui().expect("ui service bound");
    let renderers = ctx.renderers().expect("renderer service bound");
    assert!(
        !renderers.has(ServiceRendererKind::Widget, "coexist"),
        "no native registration yet"
    );

    let js = pir::tui::renderers::ExtensionRendererInvoker::new(Arc::new(handle.clone()));
    let theme = || Some(test_theme());
    let invoker = FactoryRendererInvoker::new(Some(renderers), Some(&js))
        .with_guard(Some(&handle))
        .with_theme(Some(&theme));

    // JS factory registered first: it renders.
    let js_only = invoker.invoke(&widget_request("coexist", 30)).unwrap();
    assert_eq!(js_only.lines, Some(vec!["js-widget:30".to_owned()]));

    // A native registration at the same key wins (native-first at the slot).
    let native = ProbeWidget::new("native");
    ui.set_widget("coexist", native.clone(), WidgetPlacement::AboveEditor);
    let _ = handle.take_notifications(|_| true);
    let both = invoker.invoke(&widget_request("coexist", 30)).unwrap();
    assert_eq!(
        both.lines,
        Some(vec!["native:30:1".to_owned()]),
        "native wins the shared slot"
    );

    // Native removal retires only the native side: the JS factory is alive
    // and the TUI is asked to repaint the existing slot.
    ui.clear_widget("coexist");
    let notifications = handle.take_notifications(|_| true);
    assert!(
        notifications.iter().any(|notification| matches!(
            notification,
            Notification::Status { key, .. } if key == WIDGET_RENDER_REQUEST_KEY
        )),
        "native removal must request a JS repaint: {notifications:?}"
    );
    assert!(
        !notifications.iter().any(|notification| matches!(
            notification,
            Notification::Widget { id, content } if id == "coexist" && content.is_null()
        )),
        "native removal must not emit the JS removal payload: {notifications:?}"
    );
    let revealed = invoker.invoke(&widget_request("coexist", 30)).unwrap();
    assert_eq!(
        revealed.lines,
        Some(vec!["js-widget:30".to_owned()]),
        "the JS registration renders again after the native removal"
    );

    // Same contract for the single footer slot.
    let native_footer = ProbeBar::new("native-footer");
    ui.set_footer(Some(native_footer));
    let _ = handle.take_notifications(|_| true);
    let both = invoker
        .invoke(&bar_request(
            RendererKind::Footer,
            "__footer",
            80,
            json!({ "footerData": {} }),
        ))
        .unwrap();
    assert_eq!(
        both.lines,
        Some(vec!["native-footer:80".to_owned()]),
        "native footer wins the slot"
    );
    ui.set_footer(None);
    let notifications = handle.take_notifications(|_| true);
    assert!(
        notifications.iter().any(|notification| matches!(
            notification,
            Notification::Status { key, .. } if key == WIDGET_RENDER_REQUEST_KEY
        )),
        "native footer removal reveals the JS footer via a repaint: {notifications:?}"
    );
    assert!(
        !notifications
            .iter()
            .any(|notification| matches!(notification, Notification::Footer(false))),
        "native removal must not clear the JS footer: {notifications:?}"
    );
    let revealed = invoker
        .invoke(&bar_request(
            RendererKind::Footer,
            "__footer",
            80,
            json!({ "footerData": {} }),
        ))
        .unwrap();
    assert_eq!(
        revealed.lines,
        Some(vec!["js-footer:80".to_owned()]),
        "the JS footer renders again after the native removal"
    );

    drop(session);
}

// ---------------------------------------------------------------------------
// Renderer UI guard
// ---------------------------------------------------------------------------

#[test]
fn native_factory_callbacks_run_under_the_renderer_ui_guard() {
    let fixture = Fixture::new("guard");
    let renderers = fixture.renderers();
    let probe = Arc::new(GuardProbeWidget {
        handle: fixture.handle.clone(),
        ui: fixture.ui(),
        observed_has_ui: AtomicBool::new(true),
        dialog_answer: Mutex::new(None),
    });
    fixture
        .ui()
        .set_widget("guarded", probe.clone(), WidgetPlacement::AboveEditor);
    let _ = fixture.notifications();

    let js = FakeJsInvoker::default();
    let invoker = FactoryRendererInvoker::new(Some(renderers.as_ref()), Some(&js))
        .with_guard(Some(&fixture.handle));
    let outcome = invoker.invoke(&widget_request("guarded", 40)).unwrap();
    assert_eq!(outcome.lines, Some(vec!["guarded".to_owned()]));

    // The callback observed the guard: the runtime's `has_ui` was cleared for
    // the render window, so the async dialog resolved immediately with the
    // headless `None` answer instead of parking a request no thread could
    // drain.
    assert!(
        !probe.observed_has_ui.load(Ordering::SeqCst),
        "native factory callbacks must run with the renderer UI guard held"
    );
    assert_eq!(
        probe.dialog_answer.lock().unwrap().as_deref(),
        Some("Some(Ok(None))"),
        "ctx.ui.select inside a native factory resolves immediately"
    );
    assert!(
        fixture.handle.config().has_ui,
        "the runtime's has_ui is restored after the callback"
    );
    assert!(
        !fixture.handle.renderer_callback_in_flight(),
        "the guard is released when the callback returns"
    );
    assert!(
        !fixture
            .notifications()
            .iter()
            .any(|notification| matches!(notification, Notification::DialogRequest { .. })),
        "no dialog request may be parked for a native factory callback"
    );
}

// ---------------------------------------------------------------------------
// No native registrations = unchanged JS path
// ---------------------------------------------------------------------------

#[test]
fn no_native_factories_delegates_to_the_js_bridge_unchanged() {
    let fixture = Fixture::new("js-only");
    let renderers = fixture.renderers();
    assert!(!renderers.has(ServiceRendererKind::Widget, "js-w"));
    assert!(!renderers.has(ServiceRendererKind::Footer, "__footer"));
    assert!(!renderers.has(ServiceRendererKind::Header, "__header"));
    assert!(
        !renderers.has_native_factories(),
        "no native slots => the TUI keeps its JS-only refresh path"
    );

    let js = FakeJsInvoker::default();
    js.scripted("js-w", &["js-only"]);
    let invoker = FactoryRendererInvoker::new(Some(renderers.as_ref()), Some(&js))
        .with_guard(Some(&fixture.handle));

    let outcome = invoker.invoke(&widget_request("js-w", 40)).unwrap();
    assert_eq!(outcome.lines, Some(vec!["js-only".to_owned()]));
    assert_eq!(js.calls.load(Ordering::SeqCst), 1);

    let mut registry = Vec::new();
    upsert_factory_widget(&mut registry, "js-w", "above_transcript");
    assert!(refresh_factory_widgets(&mut registry, &invoker, 40));
    assert_eq!(
        factory_widget_lines(&registry, "above_transcript")[0].1,
        vec!["js-only".to_owned()],
        "the JS-only cache path is untouched"
    );
    assert_eq!(js.calls.load(Ordering::SeqCst), 2);
}
