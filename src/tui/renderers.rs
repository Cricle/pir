//! Extension renderer consumption — the TUI side of
//! `registerMessageRenderer` / `registerEntryRenderer` /
//! `registerMarkdownTransformer` / `registerToolRenderer`.
//!
//! Upstream contract (`core/extensions/types.ts`): a message/entry renderer
//! receives `(message, options, theme)` and returns a `Component` (or
//! `undefined` to keep the default rendering); a markdown transformer
//! receives `(markdown, context)` and returns the transformed markdown; a
//! tool renderer resolver runs per tool call. pir simplifies the result
//! shape across the QuickJS actor boundary: renderers return styled lines
//! (whole ANSI strings) or a transformed markdown string — see the
//! `__pir_callRenderer` prelude entry for the JS-side normalization
//! (component-like results with a `render(width)` fn are adapted).
//!
//! The first registration per customType wins for message/entry renderers
//! (enforced JS-side in the registry); tool renderers chain every resolver
//! registered for the tool name (upstream `resolveToolRenderers`); markdown
//! transformers compose in registration order (upstream
//! `applyMarkdownTransformers`). [`renderer_registered`] re-checks the
//! exported defs Rust-side.
//! Renderer callbacks run through the extension actor mailbox with a bounded
//! timeout ([`RENDERER_CALL_TIMEOUT`]); failures degrade to the default
//! rendering with a notice.
//!
//! Factory widgets/footer/header and custom message/entry renderers are
//! invoked asynchronously: the TUI event loop snapshots the dirty work and
//! schedules it through [`RenderScheduler`], a dedicated worker invokes the
//! renderer, and the result is installed back into the cache by identity +
//! request token. The loop never blocks on a renderer callback (see the
//! "Asynchronous factory / custom-entry render scheduling" section below).
//!
//! # Native + JS resolution order
//!
//! Native renderers (`RendererService`, bound by the live plugin services)
//! and JS renderers share this resolution seam. The order is fixed:
//!
//! | Kind | Order |
//! |---|---|
//! | message / entry | native first (first registration per custom type wins), then the JS first-wins registration, then the built-in rendering |
//! | tool | the JS resolver chain in load order first; if it stays unhandled, the native per-tool resolvers in registration order, then native fallbacks, then the built-in rendering |
//! | markdown | the JS transformer chain in registration order first, then native transformers in registration order |
//! | widget / footer / header | native factory first at the same slot (widget key, footer or header slot), then the JS factory, then the built-in rendering |
//!
//! Native calls run under the same [`crate::extensions::ExtensionHandle`]
//! renderer UI guard as the JS bridge (see `with_renderer_ui_guard`), so an
//! async `ctx.ui.*` dialog requested from either face resolves immediately
//! instead of parking the UI thread. A panicking native renderer is isolated
//! in the service and degrades to "unhandled", which falls through to the
//! next face exactly like a JS renderer failure.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use super::theme::Theme;
use crate::extensions::ExtensionHandle;
use crate::plugins::plugin::services::{
    FactoryRenderContext, RendererKind as ServiceRendererKind, RendererService, ThemeSnapshot,
    ToolRenderRequest,
};

/// Re-exported outcome types (defined in the extension runtime; the
/// `__pir_callRenderer` reply shape). [`RendererCallOutcome`] additionally
/// carries the tool `renderShell` choice (`RendererOutcome` alone stays the
/// shared lines/text envelope).
pub use crate::extensions::runtime::{RendererCallOutcome, RendererOutcome, ToolRenderShell};

/// Bounded wait for one renderer callback (the QuickJS interrupt deadline
/// actually aborts a runaway JS job at this timeout, so the wait is real).
/// The synchronous bridge adds one second of reply slack on top (see
/// `ExtensionHandle::invoke_renderer_sync`).
pub const RENDERER_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Which renderer registry to consult (the `state.rendererMeta` buckets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererKind {
    Message,
    Entry,
    Markdown,
    Tool,
    /// Widget component factories (`ctx.ui.setWidget(key, factory, options)`,
    /// upstream types.ts:188-193 overload 2; rpiv-todo's overlay). The TUI
    /// re-renders the factory's component through this bucket on demand.
    Widget,
    /// Footer component factory (`ctx.ui.setFooter(factory)`, upstream
    /// types.ts:195-204): the component's lines REPLACE the built-in footer
    /// (upstream clears the footer container and adds the custom child).
    Footer,
    /// Header component factory (`ctx.ui.setHeader(factory)`, upstream
    /// types.ts:206-208): the component's lines replace the built-in startup
    /// header at the top of the content.
    Header,
}

impl serde::Serialize for RendererKind {
    /// Serialized as the `rendererMeta` bucket key so the JS side can look
    /// the registry up directly.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl RendererKind {
    /// The `rendererMeta` bucket key (also the `__pir_callRenderer` kind).
    pub fn as_str(self) -> &'static str {
        match self {
            RendererKind::Message => "messageRenderers",
            RendererKind::Entry => "entryRenderers",
            RendererKind::Markdown => "markdownTransformers",
            RendererKind::Tool => "toolRenderers",
            RendererKind::Widget => "widgetRenderers",
            RendererKind::Footer => "footerRenderers",
            RendererKind::Header => "headerRenderers",
        }
    }
}

impl From<RendererKind> for ServiceRendererKind {
    /// Map the TUI's bucket to the service-layer renderer kind (the two enums
    /// intentionally live on their own faces; this is the one conversion).
    fn from(kind: RendererKind) -> Self {
        match kind {
            RendererKind::Message => ServiceRendererKind::Message,
            RendererKind::Entry => ServiceRendererKind::Entry,
            RendererKind::Markdown => ServiceRendererKind::Markdown,
            RendererKind::Tool => ServiceRendererKind::Tool,
            RendererKind::Widget => ServiceRendererKind::Widget,
            RendererKind::Footer => ServiceRendererKind::Footer,
            RendererKind::Header => ServiceRendererKind::Header,
        }
    }
}

/// One renderer invocation (serialized into `__pir_callRenderer`'s request).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RendererRequest {
    pub kind: RendererKind,
    /// Lookup key: customType (message/entry), transformer name (markdown,
    /// empty = first registered), tool name (tool).
    pub key: String,
    /// The render payload (custom message/entry JSON, `{markdown}`, tool
    /// call JSON).
    pub payload: JsonValue,
    /// Render options: `{expanded, outputPad, width, isStreaming,
    /// messageType}` per upstream `MessageRenderOptions` /
    /// `MarkdownTransformContext` / `EntryRenderOptions`.
    pub options: JsonValue,
}

/// The seam the TUI consumes; production wraps the extension actor mailbox,
/// tests inject fakes.
pub trait RendererInvoker: Send + Sync {
    /// Invoke one renderer callback. Errors (timeout, JS exception, dead
    /// actor) degrade to the default rendering at the call sites.
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome>;

    /// [`RendererInvoker::invoke`] with the full envelope. The default drops
    /// the shell metadata (a fake invoker has none); the production invoker
    /// overrides it so tool renderer replies keep their `renderShell`.
    fn invoke_with_shell(&self, request: &RendererRequest) -> anyhow::Result<RendererCallOutcome> {
        Ok(RendererCallOutcome::from_outcome(self.invoke(request)?))
    }
}

/// Whether the exported renderer registry has a registration for `kind` /
/// `key` (pure; consumed before paying a mailbox round-trip).
///
/// The markdown bucket is keyed by name; an empty `key` matches any entry
/// (transformers compose in `__pir_callRenderer`, so the caller only needs to
/// know whether at least one is registered).
pub fn renderer_registered(defs: &JsonValue, kind: RendererKind, key: &str) -> bool {
    defs.get(kind.as_str())
        .and_then(JsonValue::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| match entry.get(kind.key_field()) {
                    Some(value) => key.is_empty() || value.as_str().is_some_and(|name| name == key),
                    None => false,
                })
        })
}

impl RendererKind {
    /// The def field carrying the lookup key in the exported registry
    /// (`customType` for message/entry/widget, `name` for markdown,
    /// `toolName` for tool renderers).
    fn key_field(self) -> &'static str {
        match self {
            RendererKind::Message | RendererKind::Entry | RendererKind::Widget => "customType",
            RendererKind::Markdown => "name",
            RendererKind::Tool => "toolName",
            // The footer/header buckets are single-slot; the key is the fixed
            // slot id and never looked up by field.
            RendererKind::Footer | RendererKind::Header => "key",
        }
    }
}

// ---------------------------------------------------------------------------
// Native renderer resolution (the `RendererService` side of the seam)
//
// These helpers are the TUI-facing resolver for native registrations: the
// caller first checks the native registry and, when an outcome is returned,
// renders its lines/text FIRST; `None` means "not handled natively — keep the
// JS/default path". A panicking native renderer is isolated inside the
// service and surfaces here as `None` (the fallback), never as an unwind.
// ---------------------------------------------------------------------------

/// Native message-renderer lines for `custom_type`, when one is registered
/// and handles the message. `None` = fall back to JS/built-in.
pub fn native_message_lines(
    service: &dyn RendererService,
    custom_type: &str,
    message: &JsonValue,
    width: usize,
) -> Option<Vec<String>> {
    if !service.has(ServiceRendererKind::Message, custom_type) {
        return None;
    }
    let outcome = service.render_message(custom_type, message, width);
    if outcome.is_unhandled() {
        return None;
    }
    outcome.lines
}

/// Native entry-renderer lines for `custom_type`; `None` = fall back.
pub fn native_entry_lines(
    service: &dyn RendererService,
    custom_type: &str,
    entry: &JsonValue,
    width: usize,
) -> Option<Vec<String>> {
    if !service.has(ServiceRendererKind::Entry, custom_type) {
        return None;
    }
    let outcome = service.render_entry(custom_type, entry, width);
    if outcome.is_unhandled() {
        return None;
    }
    outcome.lines
}

/// Native tool-renderer lines for one call; `None` = fall back to the JS
/// chain / built-in rendering.
pub fn native_tool_lines(
    service: &dyn RendererService,
    request: &ToolRenderRequest<'_>,
) -> Option<Vec<String>> {
    native_tool_outcome(service, request).and_then(|outcome| outcome.outcome.lines)
}

/// Native tool-renderer envelope for one call: `None` = no native resolver
/// handled it (the caller falls back to the JS chain / built-in rendering).
/// The envelope carries the registration's `renderShell` choice.
pub fn native_tool_outcome(
    service: &dyn RendererService,
    request: &ToolRenderRequest<'_>,
) -> Option<RendererCallOutcome> {
    if !service.has(ServiceRendererKind::Tool, request.tool_name) {
        return None;
    }
    let outcome = service.render_tool_with_shell(request);
    if outcome.is_unhandled() {
        return None;
    }
    Some(outcome)
}

/// Apply the native markdown transformer chain to `markdown`; `None` = no
/// transformer changed the text.
pub fn native_markdown(service: &dyn RendererService, markdown: &str) -> Option<String> {
    service.transform_markdown(markdown)
}

/// Production invoker: round-trips the request through the extension actor
/// mailbox with the bounded [`RENDERER_CALL_TIMEOUT`].
pub struct ExtensionRendererInvoker {
    handle: Arc<ExtensionHandle>,
}

impl ExtensionRendererInvoker {
    pub fn new(handle: Arc<ExtensionHandle>) -> Self {
        Self { handle }
    }
}

impl RendererInvoker for ExtensionRendererInvoker {
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome> {
        self.invoke_with_shell(request).map(|result| result.outcome)
    }

    fn invoke_with_shell(&self, request: &RendererRequest) -> anyhow::Result<RendererCallOutcome> {
        let request_json = serde_json::to_value(request)?;
        // The bridge also suppresses the runtime's async `ctx.ui.*` dialogs
        // for the callback's duration: this call runs on the TUI main thread,
        // so a dialog requested by the callback could never be drained and
        // would park until the renderer deadline aborted it.
        self.handle
            .invoke_renderer_sync_full(&request_json, RENDERER_CALL_TIMEOUT)
    }
}

/// The factory-slot invoker: native widget/footer/header factories first at
/// the same slot, then the JS renderer bridge.
///
/// The TUI's `FactoryWidget` caches and refresh helpers do not know which
/// face owns a slot: they call this invoker for every dirty entry, so both
/// faces share one cache, one dirty/width/theme invalidation path and one
/// failure-isolation rule. A native factory call runs under the SAME
/// [`ExtensionHandle::with_renderer_ui_guard`] as `invoke_renderer_sync`, so a
/// dialog requested from the callback resolves immediately instead of parking
/// a request only the blocked TUI thread could drain. A native factory that
/// panics is isolated inside `RendererService` (the outcome is `None`); the
/// invoker then delegates to the JS face exactly like an unhandled JS
/// renderer.
///
/// `theme` and `request_render` are built lazily: a clean slot never pays for
/// a theme snapshot, and the repaint hook queues the same reserved status key
/// the JS `tui.requestRender()` stub pushes.
pub struct FactoryRendererInvoker<'a> {
    native: Option<&'a dyn RendererService>,
    js: Option<&'a dyn RendererInvoker>,
    guard: Option<&'a ExtensionHandle>,
    theme: Option<&'a (dyn Fn() -> Option<ThemeSnapshot> + Send + Sync)>,
    request_render: Option<&'a (dyn Fn() + Send + Sync)>,
}

/// Which native factory slot a request resolved to.
#[derive(Clone, Copy)]
enum NativeFactorySlot {
    Widget,
    Footer,
    Header,
}

impl<'a> FactoryRendererInvoker<'a> {
    /// Build the composite over the native service and the JS bridge. Either
    /// side may be absent (a JS-only or native-only host).
    pub fn new(
        native: Option<&'a dyn RendererService>,
        js: Option<&'a dyn RendererInvoker>,
    ) -> Self {
        Self {
            native,
            js,
            guard: None,
            theme: None,
            request_render: None,
        }
    }

    /// Run native factory callbacks under the renderer UI guard of `guard`.
    pub fn with_guard(mut self, guard: Option<&'a ExtensionHandle>) -> Self {
        self.guard = guard;
        self
    }

    /// Provide the live theme snapshot builder (invoked only for a native
    /// render).
    pub fn with_theme(
        mut self,
        theme: Option<&'a (dyn Fn() -> Option<ThemeSnapshot> + Send + Sync)>,
    ) -> Self {
        self.theme = theme;
        self
    }

    /// Provide the `tui.requestRender()` hook handed to native factories.
    pub fn with_request_render(
        mut self,
        request_render: Option<&'a (dyn Fn() + Send + Sync)>,
    ) -> Self {
        self.request_render = request_render;
        self
    }

    fn guarded<T>(&self, render: impl FnOnce() -> T) -> T {
        match self.guard {
            Some(guard) => guard.with_renderer_ui_guard(render),
            None => render(),
        }
    }
}

impl RendererInvoker for FactoryRendererInvoker<'_> {
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome> {
        let width = request
            .options
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .map(|width| width as usize)
            .unwrap_or(80)
            .max(1);
        // Resolve the native slot first; the context (and its lazy theme
        // snapshot) is built only when a native factory actually renders, so
        // a JS-only slot keeps the pre-parity cost.
        let native_slot = self.native.and_then(|service| {
            let slot = match request.kind {
                RendererKind::Widget if service.has(ServiceRendererKind::Widget, &request.key) => {
                    Some(NativeFactorySlot::Widget)
                }
                RendererKind::Footer if service.has(ServiceRendererKind::Footer, &request.key) => {
                    Some(NativeFactorySlot::Footer)
                }
                RendererKind::Header if service.has(ServiceRendererKind::Header, &request.key) => {
                    Some(NativeFactorySlot::Header)
                }
                _ => None,
            };
            slot.map(|slot| (service, slot))
        });
        let native_lines = native_slot.and_then(|(service, slot)| {
            let theme = self.theme.and_then(|build| build());
            let context = FactoryRenderContext {
                width,
                theme: theme.as_ref(),
                footer_data: request.options.get("footerData"),
                request_render: self.request_render.map(|render| render as &dyn Fn()),
            };
            self.guarded(|| match slot {
                NativeFactorySlot::Widget => service.render_widget(&request.key, &context),
                NativeFactorySlot::Footer => service.render_footer(&context),
                NativeFactorySlot::Header => service.render_header(&context),
            })
        });
        if let Some(lines) = native_lines {
            return Ok(RendererOutcome {
                lines: Some(lines),
                text: None,
            });
        }
        match self.js {
            Some(js) => js.invoke(request),
            // No face handled the slot: the refresh helper stores empty lines
            // (the same degradation a failing JS factory gets).
            None => Ok(RendererOutcome::default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Widget component factories (`ctx.ui.setWidget(key, factory, options)`,
// upstream types.ts:188-193 overload 2 — rpiv-todo's todo overlay).
//
// The JS factory is created once per registration (prelude
// `widgetRenderers`); each TUI refresh re-invokes `component.render(width)`
// through the `__pir_callRenderer` widget round-trip and caches the returned
// lines here. The TUI never re-renders a clean widget per frame (a QuickJS
// round-trip per keystroke would be prohibitive): refreshes happen on
// registration, on the component's `tui.requestRender()` signal (the
// reserved `__pir_request_render` status key), on theme changes (upstream
// invalidates components on theme changes), and after a width change.
// ---------------------------------------------------------------------------

/// The reserved `setStatus` key the prelude's `tui.requestRender()` stub
/// pushes; the TUI drain intercepts it as a factory-widget repaint signal
/// (it never reaches the status line). Shared with the native factory
/// contexts ([`crate::plugins::plugin::services::FACTORY_RENDER_REQUEST_KEY`]).
pub const WIDGET_RENDER_REQUEST_KEY: &str =
    crate::plugins::plugin::services::FACTORY_RENDER_REQUEST_KEY;

/// One factory-backed extension widget (upstream widget-slot entries).
#[derive(Debug, Clone, PartialEq)]
pub struct FactoryWidget {
    /// The `setWidget` key (`rpiv-todos` for rpiv-todo).
    pub key: String,
    /// "above_transcript" | "below_transcript" (prelude-normalized).
    pub placement: String,
    /// Last rendered lines (whole ANSI strings). Empty until the first
    /// successful render; a factory failure clears them (that one widget
    /// hides — the failure never takes down the frame).
    pub lines: Vec<String>,
    /// Width the cached lines were rendered at (a resize re-renders).
    pub width: usize,
    /// Palette generation the cached lines were rendered under. Part of the
    /// cache key so a theme install ALWAYS re-renders the factory, even on a
    /// path that did not mark the slot dirty (a settings-menu/`ui.setTheme`
    /// swap, the theme-file hot reload, the system-theme watcher): the
    /// rendered lines may embed theme colors, so serving them under a newer
    /// generation is stale output, not a cache hit.
    pub theme_generation: u64,
    /// Set on registration / `tui.requestRender()` / theme change; consumed
    /// by [`refresh_factory_widgets`].
    pub dirty: bool,
}

/// Register (or re-place) a factory-backed widget; the next refresh renders
/// it. A registration replaces any cached lines (the factory may render
/// differently under its new options).
pub fn upsert_factory_widget(registry: &mut Vec<FactoryWidget>, key: &str, placement: &str) {
    if let Some(widget) = registry.iter_mut().find(|widget| widget.key == key) {
        widget.placement = placement.to_string();
        widget.lines.clear();
        widget.width = 0;
        widget.theme_generation = 0;
        widget.dirty = true;
        return;
    }
    registry.push(FactoryWidget {
        key: key.to_string(),
        placement: placement.to_string(),
        lines: Vec::new(),
        width: 0,
        theme_generation: 0,
        dirty: true,
    });
}

/// Remove a widget (static re-registration or `setWidget(key, undefined)`).
pub fn remove_factory_widget(registry: &mut Vec<FactoryWidget>, key: &str) {
    registry.retain(|widget| widget.key != key);
}

/// Mark every widget dirty (the `tui.requestRender()` repaint signal and
/// theme changes re-render all factory widgets, matching upstream's
/// requestRender / invalidate-on-theme-change).
pub fn mark_factory_widgets_dirty(registry: &mut [FactoryWidget]) {
    for widget in registry.iter_mut() {
        widget.dirty = true;
    }
}

/// The cached (key, lines) pairs for one dock slot, in registration order.
pub fn factory_widget_lines(
    registry: &[FactoryWidget],
    placement: &str,
) -> Vec<(String, Vec<String>)> {
    registry
        .iter()
        .filter(|widget| widget.placement == placement)
        .map(|widget| (widget.key.clone(), widget.lines.clone()))
        .collect()
}

/// Re-render dirty widgets (or those whose cached width no longer matches)
/// through the renderer callback. Returns `true` when any widget's lines
/// changed — the caller marks the frame dirty. A failing factory degrades to
/// empty lines for that widget (isolated: one broken overlay never takes
/// down the TUI frame, matching the other renderer-consumption degradations).
pub fn refresh_factory_widgets(
    registry: &mut [FactoryWidget],
    invoker: &dyn RendererInvoker,
    width: usize,
) -> bool {
    let mut changed = false;
    // One palette generation per refresh pass (a theme installed mid-pass is
    // picked up by the next frame's compare, never served as a hit).
    let generation = Theme::generation();
    for widget in registry.iter_mut() {
        if !widget.dirty && widget.width == width && widget.theme_generation == generation {
            continue;
        }
        widget.dirty = false;
        widget.width = width;
        widget.theme_generation = generation;
        let request = RendererRequest {
            kind: RendererKind::Widget,
            key: widget.key.clone(),
            payload: JsonValue::Null,
            options: serde_json::json!({ "width": width }),
        };
        let lines = match invoker.invoke(&request) {
            Ok(outcome) => outcome.lines.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if lines != widget.lines {
            widget.lines = lines;
            changed = true;
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Single factory slots: the extension footer and header
// (`ctx.ui.setFooter` / `ctx.ui.setHeader`, prelude `footerFactory` /
// `headerFactory`) replace the built-in footer / startup header with the
// component's rendered lines. Unlike widgets there is exactly one slot each;
// the LAST registration wins (upstream replaces the container child).
// ---------------------------------------------------------------------------

/// Insert a single factory slot (or clear it). A fresh/updated slot is marked
/// dirty so the follow-up [`refresh_factory_slot`] renders it; `false` clears
/// the slot (the built-in component renders again).
pub fn set_factory_slot(slot: &mut Option<FactoryWidget>, key: &str, active: bool) {
    if !active {
        *slot = None;
        return;
    }
    match slot {
        Some(widget) => {
            widget.dirty = true;
            widget.width = 0;
            widget.theme_generation = 0;
        }
        None => {
            *slot = Some(FactoryWidget {
                key: key.to_string(),
                // Single-slot bars have no placement; kept for the shared
                // FactoryWidget shape.
                placement: "bar".to_string(),
                lines: Vec::new(),
                width: 0,
                theme_generation: 0,
                dirty: true,
            });
        }
    }
}

/// Mark a factory slot dirty (the component's `requestRender()` repaint
/// signal, a theme swap, or a git-branch change for the footer).
pub fn mark_factory_slot_dirty(slot: &mut Option<FactoryWidget>) {
    if let Some(widget) = slot.as_mut() {
        widget.dirty = true;
    }
}

/// Whether a factory bar slot needs a render at `width`: registered AND
/// (dirty, cached lines rendered at another width, or rendered under another
/// palette generation). The single source of truth for the skip
/// [`refresh_factory_slot`] applies — the caller consults it BEFORE building
/// the request's options, so a clean slot costs no snapshot at all.
pub fn factory_slot_needs_render(slot: &Option<FactoryWidget>, width: usize) -> bool {
    let generation = Theme::generation();
    slot.as_ref().is_some_and(|widget| {
        widget.dirty || widget.width != width || widget.theme_generation != generation
    })
}

/// Refresh the registered factory bars, building each request's options only
/// for a bar that needs one.
///
/// Invariant: `footer_data`'s VALUE is a pure snapshot of app state, so
/// skipping its construction for a clean slot cannot change what is rendered —
/// the callee would have discarded the snapshot unread. Before this hoist the
/// per-tick refresh built the whole footer snapshot (git branch clone + a
/// JSON map of every extension status + the provider count, which clones the
/// scoped-model refs) on EVERY event-loop tick while a `ctx.ui.setFooter`
/// factory was registered, even though the slot answers from its cache on all
/// but the first/dirty/resized ticks.
pub fn refresh_factory_bars(
    footer: &mut Option<FactoryWidget>,
    header: &mut Option<FactoryWidget>,
    invoker: &dyn RendererInvoker,
    width: usize,
    footer_data: impl FnOnce() -> serde_json::Value,
) -> bool {
    let footer_needs = factory_slot_needs_render(footer, width);
    let header_needs = factory_slot_needs_render(header, width);
    if !footer_needs && !header_needs {
        return false;
    }
    let mut changed = false;
    if footer_needs
        && refresh_factory_slot(footer, RendererKind::Footer, invoker, width, footer_data())
    {
        changed = true;
    }
    if header_needs
        && refresh_factory_slot(
            header,
            RendererKind::Header,
            invoker,
            width,
            serde_json::Value::Null,
        )
    {
        changed = true;
    }
    changed
}

/// Re-render a dirty factory slot (or one whose cached width no longer
/// matches) through the renderer callback. `extra` carries the per-kind
/// render options (the footer's `footerData` snapshot); `width` is merged in
/// on top. Returns `true` when the cached lines changed. A failing factory
/// degrades to empty lines for that bar (isolated, like the widgets).
pub fn refresh_factory_slot(
    slot: &mut Option<FactoryWidget>,
    kind: RendererKind,
    invoker: &dyn RendererInvoker,
    width: usize,
    extra: JsonValue,
) -> bool {
    if !factory_slot_needs_render(slot, width) {
        return false;
    }
    let Some(widget) = slot.as_mut() else {
        // Unreachable: a slot that needs a render is registered. Kept as the
        // total form of the same check (no unwrap).
        return false;
    };
    widget.dirty = false;
    widget.width = width;
    widget.theme_generation = Theme::generation();
    let mut options = match extra {
        JsonValue::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    options.insert("width".to_string(), JsonValue::from(width));
    let request = RendererRequest {
        kind,
        key: widget.key.clone(),
        payload: JsonValue::Null,
        options: JsonValue::Object(options),
    };
    let lines = match invoker.invoke(&request) {
        Ok(outcome) => outcome.lines.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if lines != widget.lines {
        widget.lines = lines;
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Asynchronous factory / custom-entry render scheduling (W10-D)
//
// The event loop must never block on a renderer callback: a slow extension
// actor (a JS component renderer can run up to its 3.05 s deadline) or a
// hung native factory used to stall input polling for seconds per tick. The
// loop now only SNAPSHOTS the work list under the factory registry guards,
// hands the requests to a per-key latest-wins scheduler and paints the
// cached (or default) lines from the previous frame. A dedicated worker
// thread renders and posts the result back as a worker event; the loop
// installs it in place, keyed by the slot identity and the request token
// (a stale result is dropped).
//
// Coalescing: all factory requests share one dispatch window
// ([`FACTORY_RENDER_COALESCE_WINDOW`]); N `tui.requestRender()` signals
// arriving inside it produce ONE render of each affected slot, with the
// latest inputs. Unchanged inputs (same key + width + theme generation, no
// new dirty signal) are never re-requested, so a steady widget set costs
// zero factory calls per frame.
// ---------------------------------------------------------------------------

/// Minimum spacing between two factory render batches. A 500 ms
/// `tui.requestRender()` stream (pi-subagents' fleet widget) therefore
/// produces at most one render per widget per window, not one per signal.
pub const FACTORY_RENDER_COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// Bounded wait for one native factory callback. Native factories are plain
/// Rust vtables with no internal deadline; the worker races them against
/// this timeout and keeps the previous cached lines when they overrun.
pub const NATIVE_FACTORY_RENDER_TIMEOUT: Duration = Duration::from_millis(250);

/// Identity of one factory-backed slot (widget key, footer or header).
/// Results are installed by this key; a re-registration or removal bumps the
/// slot generation so results for the old identity are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FactorySlotKey {
    Widget(String),
    Footer,
    Header,
}

impl FactorySlotKey {
    pub fn widget(key: impl Into<String>) -> Self {
        Self::Widget(key.into())
    }

    /// The renderer kind (`widgetRenderers` / `footerRenderers` /
    /// `headerRenderers` bucket).
    pub fn kind(&self) -> RendererKind {
        match self {
            FactorySlotKey::Widget(_) => RendererKind::Widget,
            FactorySlotKey::Footer => RendererKind::Footer,
            FactorySlotKey::Header => RendererKind::Header,
        }
    }

    /// The `RendererRequest::key` (`customType` for widgets, the fixed slot
    /// id for bars).
    pub fn request_key(&self) -> &str {
        match self {
            FactorySlotKey::Widget(key) => key,
            FactorySlotKey::Footer => "__footer",
            FactorySlotKey::Header => "__header",
        }
    }

    /// Stable label for diagnostics / the native in-flight guard.
    pub fn label(&self) -> String {
        match self {
            FactorySlotKey::Widget(key) => format!("widget:{key}"),
            FactorySlotKey::Footer => "footer".to_string(),
            FactorySlotKey::Header => "header".to_string(),
        }
    }

    /// The `__pir_request_render` status payload this slot's
    /// `tui.requestRender()` pushes. The prelude/native hooks send this as
    /// the status TEXT so the drain can dirty exactly one slot.
    pub fn request_payload(&self) -> String {
        match self {
            FactorySlotKey::Widget(key) => {
                serde_json::json!({ "kind": "widget", "key": key }).to_string()
            }
            FactorySlotKey::Footer => r#"{"kind":"footer"}"#.to_string(),
            FactorySlotKey::Header => r#"{"kind":"header"}"#.to_string(),
        }
    }
}

/// Parse a `__pir_request_render` status payload into the slots it targets.
///
/// `None` means "legacy broadcast": an absent or unreadable payload (the
/// native `push_factory_repaint` path and older producers) dirties every
/// factory slot, preserving the pre-W10-D behavior for those callers.
pub fn parse_factory_render_request(text: Option<&str>) -> Option<Vec<FactorySlotKey>> {
    let text = text?;
    let value: JsonValue = serde_json::from_str(text).ok()?;
    match value.get("kind").and_then(JsonValue::as_str)? {
        "widget" => {
            let key = value.get("key").and_then(JsonValue::as_str)?;
            Some(vec![FactorySlotKey::Widget(key.to_string())])
        }
        "footer" => Some(vec![FactorySlotKey::Footer]),
        "header" => Some(vec![FactorySlotKey::Header]),
        _ => None,
    }
}

/// Mark ONE widget dirty (the key-scoped `tui.requestRender()` path).
/// Returns whether the widget is registered.
pub fn mark_factory_widget_dirty(registry: &mut [FactoryWidget], key: &str) -> bool {
    if let Some(widget) = registry.iter_mut().find(|widget| widget.key == key) {
        widget.dirty = true;
        return true;
    }
    false
}

/// Install one completed widget render into the registry, in place (the
/// registry order is the paint order and must not move). `lines = None`
/// records the inputs as attempted while KEEPING the previous lines (the
/// native-timeout contract); `Some(lines)` replaces them.
///
/// Returns whether the painted bytes changed.
pub fn install_factory_widget_lines(
    registry: &mut [FactoryWidget],
    key: &str,
    width: usize,
    theme_generation: u64,
    lines: Option<Vec<String>>,
) -> bool {
    let Some(widget) = registry.iter_mut().find(|widget| widget.key == key) else {
        return false;
    };
    widget.dirty = false;
    widget.width = width;
    widget.theme_generation = theme_generation;
    match lines {
        Some(lines) if lines != widget.lines => {
            widget.lines = lines;
            true
        }
        _ => false,
    }
}

/// Install one completed footer/header render into its single slot. Same
/// contract as [`install_factory_widget_lines`].
pub fn install_factory_slot_lines(
    slot: &mut Option<FactoryWidget>,
    width: usize,
    theme_generation: u64,
    lines: Option<Vec<String>>,
) -> bool {
    let Some(widget) = slot.as_mut() else {
        return false;
    };
    widget.dirty = false;
    widget.width = width;
    widget.theme_generation = theme_generation;
    match lines {
        Some(lines) if lines != widget.lines => {
            widget.lines = lines;
            true
        }
        _ => false,
    }
}

/// Snapshot the widget keys that need a render at `(width,
/// theme_generation)`. The registry guard is confined to this function and
/// DROPPED before `on_request` runs, so a renderer invoked from the callback
/// can never execute under `factory_widgets` (the W10-D lock contract).
/// Returns the number of requests passed to the callback.
pub fn collect_factory_widget_render_keys(
    registry: &Arc<Mutex<Vec<FactoryWidget>>>,
    width: usize,
    theme_generation: u64,
    mut on_request: impl FnMut(&str),
) -> usize {
    let keys: Vec<String> = {
        let widgets = lock(registry);
        widgets
            .iter()
            .filter(|widget| {
                widget.dirty || widget.width != width || widget.theme_generation != theme_generation
            })
            .map(|widget| widget.key.clone())
            .collect()
    };
    let count = keys.len();
    for key in keys {
        on_request(&key);
    }
    count
}

/// Whether the factory footer/header slots need a render at `(width,
/// theme_generation)`. Each slot guard is released before returning; the
/// caller builds the footer `footerData` snapshot only when the footer
/// answered `true`.
pub fn factory_bar_slots_need_render(
    footer: &Arc<Mutex<Option<FactoryWidget>>>,
    header: &Arc<Mutex<Option<FactoryWidget>>>,
    width: usize,
    theme_generation: u64,
) -> (bool, bool) {
    let footer_needs = {
        let slot = lock(footer);
        slot.as_ref().is_some_and(|widget| {
            widget.dirty || widget.width != width || widget.theme_generation != theme_generation
        })
    };
    let header_needs = {
        let slot = lock(header);
        slot.as_ref().is_some_and(|widget| {
            widget.dirty || widget.width != width || widget.theme_generation != theme_generation
        })
    };
    (footer_needs, header_needs)
}

/// The render inputs a cached result answers. `expanded` is always `false`
/// for factory slots; custom message/entry renderers consume the Ctrl+O
/// posture and therefore key on it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderInputs {
    pub width: usize,
    pub theme_generation: u64,
    pub expanded: bool,
}

impl RenderInputs {
    /// Factory-slot inputs (`key + width + theme generation`).
    pub fn factory(width: usize, theme_generation: u64) -> Self {
        Self {
            width,
            theme_generation,
            expanded: false,
        }
    }

    /// Custom message/entry renderer inputs (adds the expansion posture).
    pub fn entry(width: usize, theme_generation: u64, expanded: bool) -> Self {
        Self {
            width,
            theme_generation,
            expanded,
        }
    }
}

/// One scheduled render request: the slot identity, the latest-wins token,
/// the inputs the lines must be built for, and the optional snapshot the
/// renderer needs (footer `footerData`, custom-entry payload).
#[derive(Debug, Clone)]
pub struct ScheduledRender<K> {
    pub key: K,
    pub kind: RendererKind,
    pub token: u64,
    pub inputs: RenderInputs,
    pub data: Option<Arc<JsonValue>>,
}

#[derive(Debug, Default)]
struct SlotSchedule {
    /// Bumped by every dirty signal for this slot (registration,
    /// `tui.requestRender`, theme change).
    signal: u64,
    /// The signal value the latest issued request was made for.
    requested_signal: Option<u64>,
    /// The inputs of the latest issued request.
    issued: Option<RenderInputs>,
    /// Generation of the latest issued request; a completed result only
    /// installs while its token still matches.
    current_token: u64,
    /// A request is queued or in flight.
    busy: bool,
}

struct SchedulerState<K> {
    slots: HashMap<K, SlotSchedule>,
    /// Latest request per key, drained on the next dispatch window.
    queued: BTreeMap<K, ScheduledRender<K>>,
    last_dispatch: Option<Instant>,
    next_token: u64,
}

/// Per-key latest-wins request scheduler shared by the factory and
/// custom-entry render paths.
///
/// Thread-safety: the loop thread calls [`Self::request`],
/// [`Self::take_due`], [`Self::accept`] and [`Self::invalidate`]; the render
/// worker never touches it (it only receives jobs and posts results).
pub struct RenderScheduler<K> {
    state: Mutex<SchedulerState<K>>,
    window: Duration,
}

impl<K> RenderScheduler<K>
where
    K: Clone + Eq + std::hash::Hash + Ord,
{
    pub fn new(window: Duration) -> Self {
        Self {
            state: Mutex::new(SchedulerState {
                slots: HashMap::new(),
                queued: BTreeMap::new(),
                last_dispatch: None,
                next_token: 0,
            }),
            window,
        }
    }

    /// Record one dirty signal (registration / `tui.requestRender` / theme
    /// change) for a slot without issuing a render yet.
    pub fn note_dirty(&self, key: &K) {
        let mut state = lock(&self.state);
        state.slots.entry(key.clone()).or_default().signal += 1;
    }

    /// The slot's current dirty-signal value (snapshotted by the caller when
    /// it issues a request).
    pub fn signal(&self, key: &K) -> u64 {
        let state = lock(&self.state);
        state.slots.get(key).map_or(0, |slot| slot.signal)
    }

    /// Drop every outstanding/in-flight result for `key` (a removal,
    /// re-registration or identity change). The NEXT request gets a fresh
    /// token, so a late result for the old identity can never install.
    pub fn invalidate(&self, key: &K) {
        let mut state = lock(&self.state);
        let slot = state.slots.entry(key.clone()).or_default();
        slot.current_token += 1;
        slot.issued = None;
        slot.requested_signal = None;
        slot.busy = false;
        state.queued.remove(key);
    }

    /// Whether the exact request `(inputs, signal)` is already queued/in
    /// flight for `key`. Callers use this to skip building expensive request
    /// snapshots (the footer's `footerData`) while a render is outstanding.
    pub fn is_satisfied(&self, key: &K, inputs: RenderInputs, signal: u64) -> bool {
        let state = lock(&self.state);
        state.slots.get(key).is_some_and(|slot| {
            slot.busy && slot.issued == Some(inputs) && slot.requested_signal == Some(signal)
        })
    }

    /// Issue (or refresh) a request for `key`. Returns `false` when the exact
    /// same request is already queued/in flight (same inputs + same dirty
    /// signal) — the per-tick dedupe that keeps a steady frame free.
    pub fn request(
        &self,
        key: K,
        kind: RendererKind,
        inputs: RenderInputs,
        signal: u64,
        data: Option<Arc<JsonValue>>,
    ) -> bool {
        let mut state = lock(&self.state);
        state.next_token += 1;
        let token = state.next_token;
        let slot = state.slots.entry(key.clone()).or_default();
        if slot.busy && slot.issued == Some(inputs) && slot.requested_signal == Some(signal) {
            return false;
        }
        slot.current_token = token;
        slot.issued = Some(inputs);
        slot.requested_signal = Some(signal);
        slot.busy = true;
        state.queued.insert(
            key.clone(),
            ScheduledRender {
                key,
                kind,
                token,
                inputs,
                data,
            },
        );
        true
    }

    /// Drain the queued requests when the coalescing window elapsed. The
    /// first dispatch after startup is immediate; later batches are spaced by
    /// [`Self::window`], keeping all slots at one render per window.
    pub fn take_due(&self, now: Instant) -> Vec<ScheduledRender<K>> {
        let mut state = lock(&self.state);
        if state.queued.is_empty() {
            return Vec::new();
        }
        if let Some(last) = state.last_dispatch {
            if now.saturating_duration_since(last) < self.window {
                return Vec::new();
            }
        }
        state.last_dispatch = Some(now);
        std::mem::take(&mut state.queued).into_values().collect()
    }

    /// Accept the worker's result for `(key, token, inputs)`. A stale token
    /// (a newer request was issued) or mismatched inputs are rejected: the
    /// result never overwrites newer cached lines.
    pub fn accept(&self, key: &K, token: u64, inputs: RenderInputs) -> bool {
        let mut state = lock(&self.state);
        let Some(slot) = state.slots.get_mut(key) else {
            return false;
        };
        if !slot.busy || slot.current_token != token || slot.issued != Some(inputs) {
            return false;
        }
        slot.busy = false;
        true
    }

    /// Put a job back after a full worker queue (the request keeps its
    /// token; the next window retries it).
    pub fn requeue(&self, job: ScheduledRender<K>) {
        let mut state = lock(&self.state);
        if state
            .slots
            .get(&job.key)
            .is_some_and(|slot| slot.current_token == job.token)
        {
            state.queued.insert(job.key.clone(), job);
        }
    }

    /// Whether any request is queued or in flight (the loop keeps the active
    /// poll cadence while true so a coalesced request still dispatches and a
    /// completed result lands promptly).
    pub fn has_work(&self) -> bool {
        let state = lock(&self.state);
        !state.queued.is_empty() || state.slots.values().any(|slot| slot.busy)
    }

    /// Number of queued (not yet dispatched) requests. Diagnostics/tests.
    pub fn queued_len(&self) -> usize {
        lock(&self.state).queued.len()
    }

    /// Time until the coalescing window opens, when a request is waiting.
    pub fn next_due_in(&self, now: Instant) -> Option<Duration> {
        let state = lock(&self.state);
        if state.queued.is_empty() {
            return None;
        }
        match state.last_dispatch {
            Some(last) => Some(
                self.window
                    .saturating_sub(now.saturating_duration_since(last)),
            ),
            None => Some(Duration::ZERO),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Outcome of one factory render.
#[derive(Debug, Clone)]
pub enum FactoryRenderOutcome {
    Lines(Vec<String>),
    /// The native factory exceeded [`NATIVE_FACTORY_RENDER_TIMEOUT`]: keep
    /// the previous cache lines but record the attempt's inputs (so the loop
    /// does not retry the same broken slot every tick).
    TimedOut,
    /// The JS bridge (or the executor) failed: degrade to empty lines for
    /// that slot, the pre-existing failure-isolation contract.
    Failed(String),
}

/// Outcome of one custom message/entry render.
#[derive(Debug, Clone)]
pub enum CustomRenderOutcome {
    Lines(Vec<String>),
    /// No renderer handled the payload (a stale registration): fall back to
    /// the built-in default body, exactly like the synchronous path.
    Unhandled,
    Failed(String),
}

/// Production executor for the factory worker: native factory first at the
/// slot (raced against [`NATIVE_FACTORY_RENDER_TIMEOUT`]), then the JS
/// bridge. The native service cell is read live (the worker's PluginHost
/// publishes it after the session is built).
pub struct OwnedFactoryRendererInvoker {
    native: Arc<Mutex<Option<Arc<dyn RendererService>>>>,
    js: Option<Arc<dyn RendererInvoker>>,
    guard: Option<Arc<ExtensionHandle>>,
    theme: Arc<dyn Fn() -> Option<ThemeSnapshot> + Send + Sync>,
    request_render: Option<Arc<dyn Fn(FactorySlotKey) + Send + Sync>>,
    /// One native call per slot may be abandoned on timeout; this guard
    /// keeps a hung factory from spawning one leaked thread per tick.
    native_in_flight: Arc<Mutex<HashSet<String>>>,
}

impl OwnedFactoryRendererInvoker {
    pub fn new(
        native: Arc<Mutex<Option<Arc<dyn RendererService>>>>,
        js: Option<Arc<dyn RendererInvoker>>,
        guard: Option<Arc<ExtensionHandle>>,
        theme: Arc<dyn Fn() -> Option<ThemeSnapshot> + Send + Sync>,
        request_render: Option<Arc<dyn Fn(FactorySlotKey) + Send + Sync>>,
    ) -> Self {
        Self {
            native,
            js,
            guard,
            theme,
            request_render,
            native_in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Render one factory slot off the loop thread.
    pub fn render(&self, job: &ScheduledRender<FactorySlotKey>) -> FactoryRenderOutcome {
        let kind = job.key.kind();
        let service = lock(&self.native).clone();
        if let Some(service) = service
            .filter(|service| service.has(ServiceRendererKind::from(kind), job.key.request_key()))
        {
            return self.render_native(service, job, kind);
        }
        self.render_js(job, kind)
    }

    /// Native factory call with a bounded wait. The service call runs on a
    /// short-lived helper thread so a hung vtable cannot wedge the render
    /// worker; on timeout the previous lines stay cached.
    fn render_native(
        &self,
        service: Arc<dyn RendererService>,
        job: &ScheduledRender<FactorySlotKey>,
        kind: RendererKind,
    ) -> FactoryRenderOutcome {
        let label = job.key.label();
        {
            let mut in_flight = lock(&self.native_in_flight);
            if !in_flight.insert(label.clone()) {
                // A prior call for this slot is still running (it already
                // timed out once): do not pile another thread on it.
                return FactoryRenderOutcome::TimedOut;
            }
        }
        let width = job.inputs.width.max(1);
        let theme = (self.theme)();
        let footer_data = job.data.clone();
        let request_render = self.request_render.clone();
        let request_slot = job.key.clone();
        let guard = self.guard.clone();
        let service_for_thread = Arc::clone(&service);
        let key_for_thread = job.key.request_key().to_string();
        let in_flight_cell = Arc::clone(&self.native_in_flight);
        let label_for_thread = label.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("pir-native-factory".to_owned())
            .spawn(move || {
                let render = |context: &FactoryRenderContext<'_>| match kind {
                    RendererKind::Widget => {
                        service_for_thread.render_widget(&key_for_thread, context)
                    }
                    RendererKind::Footer => service_for_thread.render_footer(context),
                    RendererKind::Header => service_for_thread.render_header(context),
                    _ => None,
                };
                let request_render = move || {
                    if let Some(callback) = request_render.as_ref() {
                        callback(request_slot.clone());
                    }
                };
                let context = FactoryRenderContext {
                    width,
                    theme: theme.as_ref(),
                    footer_data: footer_data.as_deref(),
                    request_render: Some(&request_render),
                };
                let lines = match guard.as_ref() {
                    Some(guard) => guard.with_renderer_ui_guard(|| render(&context)),
                    None => render(&context),
                };
                let _ = tx.send(lines);
                lock(&in_flight_cell).remove(&label_for_thread);
            });
        if spawned.is_err() {
            lock(&self.native_in_flight).remove(&label);
            return FactoryRenderOutcome::Failed("native factory thread spawn failed".to_string());
        }
        match rx.recv_timeout(NATIVE_FACTORY_RENDER_TIMEOUT) {
            Ok(Some(lines)) => FactoryRenderOutcome::Lines(lines),
            // Unhandled/panicking native factory: the JS face still gets its
            // turn, exactly like the synchronous resolver.
            Ok(None) => self.render_js(job, kind),
            Err(RecvTimeoutError::Timeout) => FactoryRenderOutcome::TimedOut,
            Err(RecvTimeoutError::Disconnected) => self.render_js(job, kind),
        }
    }

    fn render_js(
        &self,
        job: &ScheduledRender<FactorySlotKey>,
        kind: RendererKind,
    ) -> FactoryRenderOutcome {
        let Some(js) = self.js.as_ref() else {
            return FactoryRenderOutcome::Lines(Vec::new());
        };
        let mut options = serde_json::Map::new();
        options.insert(
            "width".to_string(),
            JsonValue::from(job.inputs.width.max(1)),
        );
        if kind == RendererKind::Footer {
            if let Some(data) = job.data.as_ref() {
                options.insert("footerData".to_string(), data.as_ref().clone());
            }
        }
        let request = RendererRequest {
            kind,
            key: job.key.request_key().to_string(),
            payload: JsonValue::Null,
            options: JsonValue::Object(options),
        };
        match js.invoke(&request) {
            Ok(outcome) => FactoryRenderOutcome::Lines(outcome.lines.unwrap_or_default()),
            Err(error) => FactoryRenderOutcome::Failed(error.to_string()),
        }
    }
}

/// Production executor for the custom message/entry worker: native first,
/// then the JS bridge. Runs entirely off the loop thread.
pub struct OwnedCustomRenderExecutor {
    native: Arc<Mutex<Option<Arc<dyn RendererService>>>>,
    js: Option<Arc<dyn RendererInvoker>>,
    guard: Option<Arc<ExtensionHandle>>,
}

impl OwnedCustomRenderExecutor {
    pub fn new(
        native: Arc<Mutex<Option<Arc<dyn RendererService>>>>,
        js: Option<Arc<dyn RendererInvoker>>,
        guard: Option<Arc<ExtensionHandle>>,
    ) -> Self {
        Self { native, js, guard }
    }

    pub fn render(&self, job: &ScheduledRender<u64>) -> CustomRenderOutcome {
        let Some(payload) = job.data.as_deref() else {
            return CustomRenderOutcome::Failed("custom entry render without a payload".into());
        };
        let custom_type = payload
            .get("customType")
            .or_else(|| payload.get("type"))
            .and_then(JsonValue::as_str)
            .unwrap_or("custom");
        if let Some(service) = lock(&self.native).clone() {
            let service_kind = ServiceRendererKind::from(job.kind);
            if service.has(service_kind, custom_type) {
                let render = || {
                    if job.kind == RendererKind::Message {
                        native_message_lines(
                            service.as_ref(),
                            custom_type,
                            payload,
                            job.inputs.width,
                        )
                    } else {
                        native_entry_lines(service.as_ref(), custom_type, payload, job.inputs.width)
                    }
                };
                let lines = match self.guard.as_ref() {
                    Some(guard) => guard.with_renderer_ui_guard(render),
                    None => render(),
                };
                if let Some(lines) = lines {
                    return CustomRenderOutcome::Lines(lines);
                }
            }
        }
        let Some(js) = self.js.as_ref() else {
            // No face handled the payload: the caller rebuilds the built-in
            // default body (the synchronous fallback).
            return CustomRenderOutcome::Unhandled;
        };
        let request = RendererRequest {
            kind: job.kind,
            key: custom_type.to_string(),
            payload: payload.clone(),
            options: serde_json::json!({
                "expanded": job.inputs.expanded,
                "width": job.inputs.width,
            }),
        };
        match js.invoke(&request) {
            Ok(outcome) if outcome.is_unhandled() => CustomRenderOutcome::Unhandled,
            Ok(outcome) => CustomRenderOutcome::Lines(outcome.lines.unwrap_or_default()),
            Err(error) => CustomRenderOutcome::Failed(error.to_string()),
        }
    }
}

/// Run one render worker loop: render every received job and publish the
/// `(job, outcome)` pair. Production spawns this on a dedicated thread; the
/// test suite runs it on a fake worker thread to prove the loop never blocks
/// on a renderer.
pub fn serve_render_jobs<J, O>(
    jobs: Receiver<J>,
    mut render: impl FnMut(&J) -> O,
    mut publish: impl FnMut(J, O),
) {
    while let Ok(job) = jobs.recv() {
        let outcome = render(&job);
        publish(job, outcome);
    }
}
