//! Floating native custom overlays (`ctx.ui.custom(factory, { overlay: true })`).
//!
//! The JS face already sends `{overlay: true, overlayOptions: {...}}` on the
//! one [`Notification::DialogRequest`] channel and the TUI parses it with
//! `tui::overlay_position::OverlayOptions::from_request`. The native
//! `UiService::custom` trait method predates that option bag (its signature
//! carries only a deadline), so the native face expresses the same request
//! through:
//!
//! - [`NativeOverlayOptions`] — the typed, engine-free twin of upstream
//!   `OverlayOptions` (width / minWidth / maxHeight / anchor / offsetX /
//!   offsetY / row / col / margin / nonCapturing), serialized to the exact
//!   wire object the TUI parses. No TUI signature changes are involved.
//! - [`NativeOverlayHandle`] — the native `OverlayHandle` (upstream
//!   `types.ts:213-228`): `getBounds()` reports the last geometry the host
//!   published, and `hide`/`setHidden`/`focus`/`unfocus` drive the SAME
//!   reserved `__pir_custom_overlay` status channel the JS prelude uses.
//! - A per-factory option side table: a native wrapper factory (built in
//!   `agent_session`, since it must implement the plugin service trait)
//!   registers its option bag during `create`, and the service consumes it
//!   immediately after the factory returns. This makes the EXISTING
//!   `UiService::custom(factory, options)` float without extending that
//!   trait's signature.
//!
//! ## Propagating `getBounds()`
//!
//! The TUI owns terminal geometry, so only it can compute the rendered
//! rectangle (`ExternalOverlay::update_bounds`). It publishes it through
//! [`ExtensionHandle::publish_native_overlay_bounds`], which writes into the
//! handle's shared cell; [`NativeOverlayHandle::get_bounds`] then reports it.
//! When the overlay stops painting — `setHidden(true)`, `hide()`, or the
//! close path (`ExternalOverlay::dispose`) — the TUI clears the same cell
//! through [`ExtensionHandle::clear_native_overlay_bounds`], so `getBounds()`
//! reports `None` while the component has no rectangle; the first visible
//! frame after `focus()` republishes the current geometry. Both are plain
//! runtime calls — no TUI signature change is needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use serde_json::{Map, Value as JsonValue};

use crate::extensions::handle::ExtensionHandle;
use crate::extensions::Notification;

/// Anchor point for a floating overlay (upstream `OverlayAnchor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeOverlayAnchor {
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

impl NativeOverlayAnchor {
    /// The wire value the TUI's `OverlayAnchor::parse` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::TopLeft => "top-left",
            Self::TopRight => "top-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomRight => "bottom-right",
            Self::TopCenter => "top-center",
            Self::BottomCenter => "bottom-center",
            Self::LeftCenter => "left-center",
            Self::RightCenter => "right-center",
        }
    }
}

/// Absolute cells or a percentage of the reference dimension (upstream
/// `SizeValue`: `number | "<n>%"`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NativeOverlaySize {
    /// A number of terminal cells.
    Cells(usize),
    /// A percentage of the reference dimension (width or height).
    Percent(f64),
}

impl NativeOverlaySize {
    /// A percentage size; non-finite values are ignored when serializing.
    pub fn percent(percent: f64) -> Self {
        Self::Percent(percent)
    }

    fn to_wire(self) -> Option<JsonValue> {
        match self {
            Self::Cells(cells) => Some(JsonValue::from(cells as u64)),
            Self::Percent(percent) if percent.is_finite() => {
                Some(JsonValue::String(format!("{percent}%")))
            }
            Self::Percent(_) => None,
        }
    }
}

/// Margin from the terminal edges (upstream `OverlayMargin`); a numeric
/// `margin` expands to all four sides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeOverlayMargin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

impl NativeOverlayMargin {
    /// The same value on all four sides.
    pub fn all(margin: usize) -> Self {
        Self {
            top: margin,
            right: margin,
            bottom: margin,
            left: margin,
        }
    }

    fn is_zero(&self) -> bool {
        self.top == 0 && self.right == 0 && self.bottom == 0 && self.left == 0
    }
}

impl From<usize> for NativeOverlayMargin {
    fn from(margin: usize) -> Self {
        Self::all(margin)
    }
}

/// Typed options of a floating native custom overlay (upstream
/// `OverlayOptions`, tui.ts:254-290).
///
/// `None`/zero/`false` fields are omitted from the wire object exactly like
/// the JS face's `JSON.stringify` drops `undefined`; the TUI applies the same
/// defaults (`anchor: center`, zero offsets, zero margin, capturing focus).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NativeOverlayOptions {
    pub width: Option<NativeOverlaySize>,
    pub min_width: Option<usize>,
    pub max_height: Option<NativeOverlaySize>,
    pub anchor: Option<NativeOverlayAnchor>,
    pub offset_x: i64,
    pub offset_y: i64,
    pub row: Option<NativeOverlaySize>,
    pub col: Option<NativeOverlaySize>,
    pub margin: NativeOverlayMargin,
    /// Don't capture keyboard focus when shown (upstream `nonCapturing`).
    pub non_capturing: bool,
}

impl NativeOverlayOptions {
    /// The `overlayOptions` wire object the TUI's
    /// `OverlayOptions::from_request` parses (camelCase keys).
    pub fn to_wire(&self) -> JsonValue {
        let mut wire = Map::new();
        if let Some(width) = self.width.and_then(NativeOverlaySize::to_wire) {
            wire.insert("width".to_owned(), width);
        }
        if let Some(min_width) = self.min_width {
            wire.insert("minWidth".to_owned(), JsonValue::from(min_width as u64));
        }
        if let Some(max_height) = self.max_height.and_then(NativeOverlaySize::to_wire) {
            wire.insert("maxHeight".to_owned(), max_height);
        }
        if let Some(anchor) = self.anchor {
            wire.insert(
                "anchor".to_owned(),
                JsonValue::String(anchor.as_str().to_owned()),
            );
        }
        if self.offset_x != 0 {
            wire.insert("offsetX".to_owned(), JsonValue::from(self.offset_x));
        }
        if self.offset_y != 0 {
            wire.insert("offsetY".to_owned(), JsonValue::from(self.offset_y));
        }
        if let Some(row) = self.row.and_then(NativeOverlaySize::to_wire) {
            wire.insert("row".to_owned(), row);
        }
        if let Some(col) = self.col.and_then(NativeOverlaySize::to_wire) {
            wire.insert("col".to_owned(), col);
        }
        if !self.margin.is_zero() {
            let margin = &self.margin;
            wire.insert(
                "margin".to_owned(),
                serde_json::json!({
                    "top": margin.top,
                    "right": margin.right,
                    "bottom": margin.bottom,
                    "left": margin.left,
                }),
            );
        }
        if self.non_capturing {
            wire.insert("nonCapturing".to_owned(), JsonValue::Bool(true));
        }
        JsonValue::Object(wire)
    }
}

/// Terminal-relative rectangle of one rendered overlay (upstream
/// `OverlayBounds`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeOverlayBounds {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
}

impl NativeOverlayBounds {
    /// The JSON shape the JS handle's `getBounds()` reports (same field
    /// names), useful for host protocol payloads.
    pub fn to_json(self) -> JsonValue {
        serde_json::json!({
            "row": self.row,
            "col": self.col,
            "width": self.width,
            "height": self.height,
        })
    }
}

/// The reserved status key the TUI intercepts as a custom-overlay control
/// event. Kept in sync with
/// [`crate::plugins::plugin::services::CUSTOM_OVERLAY_EVENT_KEY`] (the
/// extension layer cannot import the service layer; the g_agent tests pin the
/// two together).
const CUSTOM_OVERLAY_EVENT_KEY: &str = "__pir_custom_overlay";

type BoundsCell = Arc<StdMutex<Option<NativeOverlayBounds>>>;

#[derive(Default)]
struct HandleState {
    /// Dialog id allocated by the host when the request was admitted.
    id: Option<u64>,
    /// The presentation queue the overlay was opened on (hide/unhide path).
    queue: Option<ExtensionHandle>,
    /// Last geometry the host published (TUI seam).
    bounds: BoundsCell,
    hidden: bool,
    permanently_hidden: bool,
}

/// Native `OverlayHandle` for one `ctx.ui.custom` overlay (upstream
/// `OverlayHandle`, tui.ts:309-321).
///
/// Cloneable and thread-safe: the caller keeps one clone while the host binds
/// the dialog id and the TUI later publishes geometry into the same cell.
#[derive(Clone, Default)]
pub struct NativeOverlayHandle {
    state: Arc<StdMutex<HandleState>>,
}

impl std::fmt::Debug for NativeOverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bounds = state
            .bounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("NativeOverlayHandle")
            .field("id", &state.id)
            .field("hidden", &state.hidden)
            .field("bounds", &*bounds)
            .finish()
    }
}

impl NativeOverlayHandle {
    /// A fresh handle, unbound until the host admits the overlay.
    pub fn new() -> Self {
        Self::default()
    }

    /// The dialog id the host allocated, once known.
    pub fn id(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .id
    }

    /// The last terminal-relative rectangle the host published (upstream
    /// `OverlayHandle.getBounds`). `None` before the first floating render,
    /// and again after the TUI clears the cell because the overlay was hidden
    /// (`setHidden`/`hide`) or closed (see the module docs).
    pub fn get_bounds(&self) -> Option<NativeOverlayBounds> {
        let bounds = Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bounds,
        );
        let value = *bounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        value
    }

    /// Whether the overlay currently paints nothing and releases the keyboard
    /// (upstream `OverlayHandle.isHidden`).
    pub fn is_hidden(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hidden
    }

    /// Whether the overlay currently owns focus (visible; upstream
    /// `OverlayHandle.isFocused`).
    pub fn is_focused(&self) -> bool {
        !self.is_hidden()
    }

    /// `OverlayHandle.setHidden`: hide/show without permanently retiring the
    /// overlay. Showing is ignored after [`Self::hide`].
    pub fn set_hidden(&self, hidden: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.hidden == hidden || (!hidden && state.permanently_hidden) {
            return;
        }
        state.hidden = hidden;
        push_hidden(&state, hidden);
    }

    /// `OverlayHandle.hide`: hide for good (upstream's `permanentlyHidden`).
    pub fn hide(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.permanently_hidden = true;
        if !state.hidden {
            state.hidden = true;
            push_hidden(&state, true);
        }
    }

    /// `OverlayHandle.focus`: show again unless the overlay was permanently
    /// hidden.
    pub fn focus(&self) {
        self.set_hidden(false);
    }

    /// `OverlayHandle.unfocus`: hide (reversible).
    pub fn unfocus(&self) {
        self.set_hidden(true);
    }

    /// Host-side bind: record the dialog id/queue and share the bounds cell so
    /// [`ExtensionHandle::publish_native_overlay_bounds`] and
    /// [`ExtensionHandle::clear_native_overlay_bounds`] can reach it.
    /// Called by the UI service synchronously before the request is queued.
    pub(crate) fn bind(&self, id: u64, queue: &ExtensionHandle) {
        let bounds = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.id = Some(id);
            state.queue = Some(queue.clone());
            if state.hidden {
                push_hidden(&state, true);
            }
            Arc::clone(&state.bounds)
        };
        queue.register_native_overlay_bounds(id, bounds);
    }
}

/// Push one `{component, op: "hidden", hidden}` event on the reserved status
/// channel (the same shape the JS prelude's `applyHidden` produces).
fn push_hidden(state: &HandleState, hidden: bool) {
    let (Some(id), Some(queue)) = (state.id, state.queue.as_ref()) else {
        return;
    };
    queue.push_notification(Notification::Status {
        key: CUSTOM_OVERLAY_EVENT_KEY.to_owned(),
        text: Some(
            serde_json::json!({
                "component": format!("native:{id}"),
                "op": "hidden",
                "hidden": hidden,
            })
            .to_string(),
        ),
    });
}

/// One option bag registered by a native factory wrapper and consumed by the
/// UI service right after `create`.
#[derive(Clone)]
pub(crate) struct OverlayFactorySpec {
    pub options: NativeOverlayOptions,
    pub handle: NativeOverlayHandle,
}

type FactorySpecs = StdMutex<HashMap<usize, Vec<OverlayFactorySpec>>>;

fn factory_specs() -> &'static FactorySpecs {
    static SPECS: OnceLock<FactorySpecs> = OnceLock::new();
    SPECS.get_or_init(StdMutex::default)
}

/// Register one option bag for a factory's data pointer. Repeated registers
/// for the same pointer queue up (each `create` call registers once, each
/// `custom` call consumes once).
pub(crate) fn register_overlay_factory_spec(pointer: usize, spec: OverlayFactorySpec) {
    factory_specs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(pointer)
        .or_default()
        .push(spec);
}

/// Consume the oldest option bag for a factory's data pointer.
pub(crate) fn take_overlay_factory_spec(pointer: usize) -> Option<OverlayFactorySpec> {
    let mut specs = factory_specs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let spec = specs.get_mut(&pointer)?.pop();
    if specs.get(&pointer).is_some_and(Vec::is_empty) {
        specs.remove(&pointer);
    }
    spec
}

// ---------------------------------------------------------------------------
// Host bounds publishing (the TUI seam)
// ---------------------------------------------------------------------------

type BoundsMap = StdMutex<HashMap<(usize, u64), Weak<StdMutex<Option<NativeOverlayBounds>>>>>;

fn bounds_map() -> &'static BoundsMap {
    static MAP: OnceLock<BoundsMap> = OnceLock::new();
    MAP.get_or_init(StdMutex::default)
}

/// Register the bounds cell of dialog `id` for one runtime generation.
pub(crate) fn register_bounds(runtime: usize, id: u64, cell: BoundsCell) {
    let mut map = bounds_map()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.retain(|_, weak| weak.strong_count() > 0);
    map.insert((runtime, id), Arc::downgrade(&cell));
}

/// The live bounds cell of dialog `id`, pruning entries whose handle is gone.
fn live_bounds_cell(runtime: usize, id: u64) -> Option<BoundsCell> {
    let mut map = bounds_map()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.retain(|_, weak| weak.strong_count() > 0);
    map.get(&(runtime, id)).and_then(Weak::upgrade)
}

/// Publish geometry for dialog `id`. Returns whether a live handle was
/// waiting for it.
pub(crate) fn publish_bounds(runtime: usize, id: u64, bounds: NativeOverlayBounds) -> bool {
    let Some(cell) = live_bounds_cell(runtime, id) else {
        return false;
    };
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bounds);
    true
}

/// Clear the geometry of dialog `id` (the overlay stopped painting). Returns
/// whether a live handle was waiting for it.
///
/// The registration is intentionally kept: hiding is reversible, and the
/// first visible frame after showing publishes again into the same cell.
pub(crate) fn clear_bounds(runtime: usize, id: u64) -> bool {
    let Some(cell) = live_bounds_cell(runtime, id) else {
        return false;
    };
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    true
}
