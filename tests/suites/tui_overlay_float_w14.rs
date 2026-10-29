//! Floating custom overlays (audit §7 #11 residual).
//!
//! `ctx.ui.custom(factory, { overlay: true, overlayOptions })` places the
//! component on a floating terminal-relative rectangle instead of the
//! full-width editor slot (upstream `showOverlay` / `resolveOverlayLayout` /
//! `compositeOverlays`). These tests pin the option parsing, the upstream
//! geometry math, the frame composition, `getBounds()` reporting, and the
//! default editor-slot path.
//!
//! The `native_*` tail of this suite covers the native `UiService::custom`
//! seam: the UI service binds its `NativeOverlayHandle` to the dialog id and
//! the TUI publishes every computed rectangle there
//! (`ExtensionHandle::publish_native_overlay_bounds`), clearing it again when
//! the overlay is hidden or closed (`clear_native_overlay_bounds`), so
//! `get_bounds()` reports exactly the rectangle the frame compositor uses.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::extensions::{
    ExtensionConfig, ExtensionHandle, NativeOverlayAnchor, NativeOverlayBounds,
    NativeOverlayHandle, NativeOverlayOptions, NativeOverlaySize,
};
use pir::plugins::plugin::services::{FactoryRenderContext, OverlayComponent};
use pir::tui::overlay_position::{OverlayBounds, OverlayOptions, SizeValue};
use pir::tui::terminal::{
    bind_native_overlay_handle, composite_floating_overlay, external_overlay_key_route,
    external_overlay_paste_route, render_active_overlay, ExternalOverlay, ExternalPasteRoute,
    Overlay,
};

struct FakeComponent {
    last_width: AtomicUsize,
}

impl FakeComponent {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            last_width: AtomicUsize::new(0),
        })
    }
}

impl OverlayComponent for FakeComponent {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        self.last_width.store(context.width, Ordering::SeqCst);
        vec![format!("panel @{}", context.width)]
    }

    fn handle_input(&self, _data: &str) {}
}

fn overlay_with(
    component: Arc<dyn OverlayComponent>,
    options: Option<OverlayOptions>,
) -> Arc<ExternalOverlay> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let entry = ExternalOverlay::new(7, "panel", component, deadline);
    Arc::new(match options {
        Some(options) => entry.with_position(options),
        None => entry,
    })
}

/// `overlay: true` opts into floating; absent/false keeps the editor slot.
/// All upstream `OverlayOptions` fields parse.
#[test]
fn overlay_options_parse_the_upstream_fields() {
    assert!(OverlayOptions::from_request(&serde_json::json!({})).is_none());
    assert!(
        OverlayOptions::from_request(&serde_json::json!({
            "overlay": false,
            "overlayOptions": { "width": 20 }
        }))
        .is_none(),
        "overlayOptions without overlay:true is not a floating request"
    );

    let parsed = OverlayOptions::from_request(&serde_json::json!({
        "overlay": true,
        "overlayOptions": {
            "width": "50%",
            "minWidth": 30,
            "maxHeight": "40%",
            "anchor": "top-right",
            "offsetX": -2,
            "offsetY": 3,
            "margin": { "top": 1, "right": 2, "bottom": 3, "left": 4 },
            "nonCapturing": true
        }
    }))
    .expect("floating request");
    assert_eq!(parsed.width, Some(SizeValue::Percent(50.0)));
    assert_eq!(parsed.min_width, Some(30));
    assert_eq!(parsed.max_height, Some(SizeValue::Percent(40.0)));
    assert_eq!(
        parsed.anchor,
        Some(pir::tui::overlay_position::OverlayAnchor::TopRight)
    );
    assert_eq!(parsed.offset_x, -2);
    assert_eq!(parsed.offset_y, 3);
    assert_eq!(parsed.margin.top, 1);
    assert_eq!(parsed.margin.right, 2);
    assert_eq!(parsed.margin.bottom, 3);
    assert_eq!(parsed.margin.left, 4);
    assert!(parsed.non_capturing);

    // A bare `overlay: true` floats with the upstream default placement.
    let default = OverlayOptions::from_request(&serde_json::json!({ "overlay": true }))
        .expect("default floating request");
    assert_eq!(default.render_width(80), 80);
}

/// The geometry math mirrors upstream `resolveOverlayLayout`.
#[test]
fn floating_layout_matches_the_upstream_math() {
    // width "50%" + center anchor at 80x24, 6 content rows.
    let centered = OverlayOptions {
        width: Some(SizeValue::Percent(50.0)),
        ..OverlayOptions::default()
    };
    let bounds = centered.layout(80, 24, 6);
    assert_eq!(
        bounds,
        pir::tui::overlay_position::OverlayBounds {
            row: 9,
            col: 20,
            width: 40,
            height: 6
        }
    );
    assert_eq!(centered.render_width(80), 40);

    // Margin (all sides) shrinks the available box but the anchor remains
    // centered inside it.
    let margined = OverlayOptions {
        width: Some(SizeValue::Percent(50.0)),
        margin: pir::tui::overlay_position::OverlayMargin {
            top: 2,
            right: 2,
            bottom: 2,
            left: 2,
        },
        ..OverlayOptions::default()
    };
    let bounds = margined.layout(80, 24, 6);
    assert_eq!((bounds.row, bounds.col), (9, 20));

    // Absolute percentage row/col: 50% of the remaining slack.
    let positioned = OverlayOptions {
        width: Some(SizeValue::Absolute(40)),
        row: Some(SizeValue::Percent(50.0)),
        col: Some(SizeValue::Percent(50.0)),
        ..OverlayOptions::default()
    };
    let bounds = positioned.layout(80, 24, 6);
    assert_eq!((bounds.row, bounds.col), (9, 20));

    // maxHeight caps the effective height and the bottom anchor follows it.
    let bottom = OverlayOptions {
        width: Some(SizeValue::Absolute(40)),
        max_height: Some(SizeValue::Absolute(4)),
        anchor: Some(pir::tui::overlay_position::OverlayAnchor::BottomRight),
        ..OverlayOptions::default()
    };
    let bounds = bottom.layout(80, 24, 10);
    assert_eq!(bounds.height, 4);
    assert_eq!((bounds.row, bounds.col), (20, 40));

    // minWidth raises the default width; margin clamping keeps the rectangle
    // inside the terminal.
    let widened = OverlayOptions {
        min_width: Some(90),
        ..OverlayOptions::default()
    };
    assert_eq!(widened.render_width(80), 80);
    let offset = OverlayOptions {
        width: Some(SizeValue::Absolute(10)),
        offset_x: -100,
        offset_y: 100,
        margin: pir::tui::overlay_position::OverlayMargin {
            top: 1,
            right: 1,
            bottom: 1,
            left: 1,
        },
        ..OverlayOptions::default()
    };
    let bounds = offset.layout(20, 10, 2);
    assert_eq!((bounds.row, bounds.col), (7, 1));
}

/// Without `overlay: true` the component keeps the historic full-width editor
/// slot: the dock's renderer serves its lines and the frame decorator leaves
/// the frame alone.
#[test]
fn the_editor_slot_path_stays_the_default() {
    let component = FakeComponent::new();
    let entry = overlay_with(Arc::clone(&component) as Arc<dyn OverlayComponent>, None);
    assert!(!entry.is_floating());
    assert_eq!(entry.render_width(80), 80);
    entry.refresh(80, None);
    assert_eq!(component.last_width.load(Ordering::SeqCst), 80);
    assert!(entry.bounds().is_none());

    let mut overlay = Overlay::default();
    let mut base = vec!["x".repeat(80); 24];
    let original = base.clone();
    assert!(matches!(
        overlay
            .external_mut()
            .admit(Arc::clone(&entry), Instant::now()),
        Ok(true)
    ));
    assert_eq!(
        render_active_overlay(&overlay, 80),
        Some(vec!["panel @80".to_string()]),
        "the editor slot still serves the component"
    );
    composite_floating_overlay(&overlay, &mut base);
    assert_eq!(base, original, "the default slot is not frame-composited");
}

/// A floating overlay renders at its own width, reports real bounds, and is
/// composited over the painted frame at that rectangle.
#[test]
fn floating_overlays_report_bounds_and_composite_over_the_frame() {
    let component = FakeComponent::new();
    let options = OverlayOptions {
        width: Some(SizeValue::Absolute(20)),
        anchor: Some(pir::tui::overlay_position::OverlayAnchor::TopLeft),
        margin: pir::tui::overlay_position::OverlayMargin {
            top: 1,
            right: 1,
            bottom: 1,
            left: 1,
        },
        ..OverlayOptions::default()
    };
    let entry = Arc::new(
        ExternalOverlay::new(
            9,
            "float",
            Arc::clone(&component) as Arc<dyn OverlayComponent>,
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(options),
    );
    assert!(entry.is_floating());
    assert_eq!(entry.render_width(80), 20);
    entry.refresh_for_terminal(80, 24, None);
    assert_eq!(component.last_width.load(Ordering::SeqCst), 20);
    assert_eq!(
        entry.bounds(),
        Some(pir::tui::overlay_position::OverlayBounds {
            row: 1,
            col: 1,
            width: 20,
            height: 1
        })
    );

    let mut overlay = Overlay::default();
    assert!(matches!(
        overlay
            .external_mut()
            .admit(Arc::clone(&entry), Instant::now()),
        Ok(true)
    ));
    assert!(
        render_active_overlay(&overlay, 80).is_none(),
        "a floating overlay does not replace the editor slot"
    );

    let mut base = vec!["x".repeat(80); 24];
    composite_floating_overlay(&overlay, &mut base);
    assert_eq!(base[0], "x".repeat(80), "row 0 is outside the rectangle");
    assert!(
        base[1].contains("panel @20"),
        "the overlay is composited at its column: {:?}",
        &base[1][..30]
    );
    assert_eq!(base[2], "x".repeat(80));
}

/// The compositor replaces only the overlay cells: the styled text left and
/// right of the rectangle keeps its bytes and its active SGR state.
#[test]
fn compositing_preserves_the_surrounding_styled_text() {
    let mut lines = vec![format!("\x1b[31m{}\x1b[0m", "a".repeat(20))];
    let overlay = vec!["panel".to_string()];
    pir::tui::overlay_position::composite_lines(
        &mut lines,
        &overlay,
        pir::tui::overlay_position::OverlayBounds {
            row: 0,
            col: 5,
            width: 5,
            height: 1,
        },
    );
    let line = &lines[0];
    assert_eq!(
        pir::tui::ansi::strip_ansi(line),
        "aaaaapanelaaaaaaaaaa",
        "only the rectangle's cells are replaced"
    );
    assert!(line.starts_with("\x1b[31maaaaa"), "prefix style kept");
    assert!(
        line.matches("\x1b[31m").count() >= 2,
        "the SGR state is re-opened after the replacement"
    );
    assert!(
        line.contains("panel"),
        "the overlay content is present: {line:?}"
    );
}

/// `nonCapturing` floating overlays paint without owning the keyboard; a
/// hidden overlay paints nothing and loses its bounds (upstream clears
/// `entry.bounds` while hidden).
#[test]
fn non_capturing_and_hidden_floating_overlays() {
    let options = OverlayOptions {
        width: Some(SizeValue::Absolute(20)),
        non_capturing: true,
        ..OverlayOptions::default()
    };
    let entry = Arc::new(
        ExternalOverlay::new(
            11,
            "passive",
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(options),
    );
    entry.refresh_for_terminal(80, 24, None);
    let mut overlay = Overlay::default();
    assert!(matches!(
        overlay
            .external_mut()
            .admit(Arc::clone(&entry), Instant::now()),
        Ok(true)
    ));
    assert!(
        !overlay.any_open(),
        "nonCapturing overlays do not own focus"
    );
    assert_eq!(
        external_overlay_key_route(
            overlay.external(),
            &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)
        ),
        None
    );
    assert_eq!(
        external_overlay_paste_route(overlay.external(), "hello"),
        ExternalPasteRoute::None
    );
    let mut base = vec!["x".repeat(80); 24];
    composite_floating_overlay(&overlay, &mut base);
    assert!(base.iter().any(|line| line.contains("panel @20")));

    entry.set_hidden(true);
    assert_eq!(entry.bounds(), None, "hidden overlays clear their bounds");
    let mut hidden_base = vec!["x".repeat(80); 24];
    composite_floating_overlay(&overlay, &mut hidden_base);
    assert!(
        hidden_base.iter().all(|line| line == &"x".repeat(80)),
        "a hidden overlay paints nothing"
    );
}

/// The bounds cell is the `getBounds()` source: it survives across frames and
/// is recomputed for the current terminal size.
#[test]
fn get_bounds_tracks_the_last_rendered_rectangle() {
    let options = OverlayOptions {
        width: Some(SizeValue::Absolute(30)),
        anchor: Some(pir::tui::overlay_position::OverlayAnchor::Center),
        ..OverlayOptions::default()
    };
    let entry = Arc::new(
        ExternalOverlay::new(
            12,
            "bounds",
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(options),
    );
    entry.refresh_for_terminal(80, 24, None);
    let wide = entry.bounds().expect("bounds after the first frame");
    assert_eq!((wide.width, wide.height), (30, 1));
    assert_eq!((wide.row, wide.col), (11, 25));

    entry.refresh_for_terminal(60, 20, None);
    let narrow = entry.bounds().expect("bounds after a resize");
    assert_eq!((narrow.row, narrow.col), (9, 15));
}

/// A component with a fixed number of rows keeps its full height in the
/// bounds; the shared bounds cell is a plain copy (no aliasing surprises).
#[test]
fn bounds_are_a_value_copy() {
    let options = OverlayOptions {
        width: Some(SizeValue::Absolute(10)),
        ..OverlayOptions::default()
    };
    let entry = Arc::new(
        ExternalOverlay::new(
            13,
            "copy",
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(options),
    );
    entry.refresh_for_terminal(40, 10, None);
    let first = entry.bounds().expect("bounds");
    let mut copy = first;
    copy.row = 0;
    assert_eq!(entry.bounds(), Some(first));
    assert_ne!(copy, first);
}

/// No waiters may leak from these unit tests: the slot API is shared with the
/// live dialogs, so expose the deadline path once here.
#[test]
fn floating_overlay_settles_like_any_dialog() {
    let entry = Arc::new(
        ExternalOverlay::new(
            14,
            "settle",
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(OverlayOptions::default()),
    );
    let mut overlay = Overlay::default();
    assert!(matches!(
        overlay.external_mut().admit(entry, Instant::now()),
        Ok(true)
    ));
    let settled = overlay
        .external_mut()
        .settle(|_| true)
        .expect("the floating overlay settles");
    settled.dispose();
    assert!(!overlay.external().is_open());
}

// ---------------------------------------------------------------------------
// Native `UiService::custom` → `NativeOverlayHandle::getBounds()`
// ---------------------------------------------------------------------------

/// A plan-only runtime handle: the same presentation queue the UI service
/// writes its bound native-overlay cell into, without starting QuickJS.
fn plan_only_extensions() -> Arc<ExtensionHandle> {
    Arc::new(ExtensionHandle::start_plan_only(ExtensionConfig {
        mode: "interactive".to_owned(),
        has_ui: true,
        ..ExtensionConfig::default()
    }))
}

/// Build the TUI entry the way `App::open_custom_overlay` does for a native
/// request: the option bag crossed the wire (`NativeOverlayOptions::to_wire`
/// → `OverlayOptions::from_request`), the runtime handle is attached for the
/// `getBounds()` echo, and the native handle is bound to the request id (the
/// step `UiService::custom` performs before queueing the DialogRequest).
fn native_floating_entry(
    id: u64,
    options: &NativeOverlayOptions,
    extensions: &Arc<ExtensionHandle>,
    handle: &NativeOverlayHandle,
) -> Arc<ExternalOverlay> {
    bind_native_overlay_handle(handle, id, extensions);
    let request = serde_json::json!({
        "overlay": true,
        "overlayOptions": options.to_wire(),
    });
    let position = OverlayOptions::from_request(&request).expect("floating native request");
    Arc::new(
        ExternalOverlay::new(
            id,
            format!("native:{id}"),
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(position)
        .with_native_bounds(Arc::clone(extensions)),
    )
}

/// The production gap this closes: a native floating overlay's `getBounds()`
/// used to stay `None`. Every TUI layout pass now publishes the real
/// rectangle, and a resize publishes the new one.
#[test]
fn native_floating_overlay_publishes_tui_bounds_to_get_bounds() {
    let extensions = plan_only_extensions();
    let handle = NativeOverlayHandle::new();
    let options = NativeOverlayOptions {
        width: Some(NativeOverlaySize::Cells(30)),
        anchor: Some(NativeOverlayAnchor::Center),
        ..NativeOverlayOptions::default()
    };
    let entry = native_floating_entry(21, &options, &extensions, &handle);
    assert_eq!(handle.id(), Some(21), "the UI service binds the dialog id");
    assert_eq!(
        handle.get_bounds(),
        None,
        "no geometry before the first TUI layout pass"
    );

    entry.refresh_for_terminal(80, 24, None);
    let first = OverlayBounds {
        row: 11,
        col: 25,
        width: 30,
        height: 1,
    };
    assert_eq!(entry.bounds(), Some(first));
    assert_eq!(
        handle.get_bounds(),
        Some(NativeOverlayBounds {
            row: 11,
            col: 25,
            width: 30,
            height: 1,
        }),
        "getBounds() reports the rectangle the compositor paints"
    );

    entry.refresh_for_terminal(60, 20, None);
    assert_eq!(
        handle.get_bounds(),
        Some(NativeOverlayBounds {
            row: 9,
            col: 15,
            width: 30,
            height: 1,
        }),
        "a terminal resize republishes the new rectangle"
    );

    // A steady frame publishes no second value (publish happens on change).
    entry.refresh_for_terminal(60, 20, None);
    assert_eq!(
        handle.get_bounds(),
        Some(NativeOverlayBounds {
            row: 9,
            col: 15,
            width: 30,
            height: 1,
        })
    );
}

/// `OverlayHandle.setHidden` clears the TUI's stored rectangle (upstream
/// `entry.bounds`) AND the native handle's published bounds, so a hidden
/// overlay reports no geometry. Showing publishes nothing by itself; the
/// first visible layout pass republishes the rectangle for the current
/// terminal size.
#[test]
fn hidden_native_overlay_clears_entry_bounds_and_republishes_on_show() {
    let extensions = plan_only_extensions();
    let handle = NativeOverlayHandle::new();
    let options = NativeOverlayOptions {
        width: Some(NativeOverlaySize::Cells(30)),
        anchor: Some(NativeOverlayAnchor::Center),
        ..NativeOverlayOptions::default()
    };
    let entry = native_floating_entry(22, &options, &extensions, &handle);
    entry.refresh_for_terminal(80, 24, None);
    let visible = NativeOverlayBounds {
        row: 11,
        col: 25,
        width: 30,
        height: 1,
    };
    assert_eq!(handle.get_bounds(), Some(visible));

    entry.set_hidden(true);
    assert_eq!(entry.bounds(), None, "hidden overlays clear entry.bounds");
    assert_eq!(
        handle.get_bounds(),
        None,
        "hiding clears the native getBounds() cell: no stale rectangle"
    );

    // Showing alone is not a publish point; the visibility flag only
    // schedules the next layout pass.
    entry.set_hidden(false);
    assert_eq!(handle.get_bounds(), None);

    entry.refresh_for_terminal(80, 24, None);
    assert_eq!(
        handle.get_bounds(),
        Some(visible),
        "showing republishes geometry for the current size"
    );

    entry.refresh_for_terminal(100, 30, None);
    assert_eq!(
        handle.get_bounds(),
        Some(NativeOverlayBounds {
            row: 14,
            col: 35,
            width: 30,
            height: 1,
        }),
        "a later resize still republishes the new rectangle"
    );
}

/// Closing the overlay (`dispose`, every settlement path) clears the native
/// `getBounds()` cell and removes the publisher, so a settled caller never
/// observes geometry from a closed overlay.
#[test]
fn disposed_native_overlay_stops_publishing_bounds() {
    let extensions = plan_only_extensions();
    let handle = NativeOverlayHandle::new();
    let options = NativeOverlayOptions {
        width: Some(NativeOverlaySize::Cells(30)),
        anchor: Some(NativeOverlayAnchor::Center),
        ..NativeOverlayOptions::default()
    };
    let entry = native_floating_entry(23, &options, &extensions, &handle);
    entry.refresh_for_terminal(80, 24, None);
    let last = handle.get_bounds().expect("bounds after the first frame");
    assert_eq!(last.width, 30);

    entry.dispose();
    assert!(
        entry.component().is_none(),
        "dispose releases the paint/input face"
    );
    assert_eq!(
        handle.get_bounds(),
        None,
        "a closed overlay has no geometry"
    );
    entry.refresh_for_terminal(120, 40, None);
    assert_eq!(
        handle.get_bounds(),
        None,
        "a closed overlay publishes no further geometry"
    );
}

/// `clear_native_overlay_bounds` targets exactly the bound id, reports
/// whether a live handle was reached, and keeps the binding alive: a clear is
/// a hide (reversible), not a close, so the next publish still lands in the
/// same handle.
#[test]
fn clear_native_overlay_bounds_only_clears_the_bound_id() {
    let extensions = plan_only_extensions();
    let first = NativeOverlayHandle::new();
    let second = NativeOverlayHandle::new();
    bind_native_overlay_handle(&first, 30, &extensions);
    bind_native_overlay_handle(&second, 31, &extensions);

    let bounds = NativeOverlayBounds {
        row: 1,
        col: 2,
        width: 3,
        height: 4,
    };
    assert!(extensions.publish_native_overlay_bounds(30, bounds));
    assert!(extensions.publish_native_overlay_bounds(31, bounds));

    assert!(extensions.clear_native_overlay_bounds(30));
    assert_eq!(
        first.get_bounds(),
        None,
        "the cleared id reports no geometry"
    );
    assert_eq!(
        second.get_bounds(),
        Some(bounds),
        "clearing one id leaves every other overlay's geometry alone"
    );
    assert!(
        !extensions.clear_native_overlay_bounds(99),
        "an unregistered id reports no waiter, exactly like publish"
    );

    assert!(
        extensions.publish_native_overlay_bounds(30, bounds),
        "clear keeps the binding live for the next show"
    );
    assert_eq!(first.get_bounds(), Some(bounds));
}

/// The editor-slot default (no `overlay: true`) has no rectangle: the TUI
/// publishes nothing and `getBounds()` stays `None` — R2's non-floating
/// behavior is untouched.
#[test]
fn native_editor_slot_overlay_publishes_no_bounds() {
    let extensions = plan_only_extensions();
    let handle = NativeOverlayHandle::new();
    bind_native_overlay_handle(&handle, 24, &extensions);
    let entry = Arc::new(
        ExternalOverlay::new(
            24,
            "native:24",
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_native_bounds(Arc::clone(&extensions)),
    );

    entry.refresh_for_terminal(80, 24, None);
    assert_eq!(entry.bounds(), None);
    assert_eq!(handle.get_bounds(), None);
}

/// The JS face's bounds ride the render request (`JsOverlayComponent`) and
/// must not be echoed into a native handle's cell: an overlay built without
/// `with_native_bounds` publishes nothing even when the id is bound.
#[test]
fn js_face_overlay_does_not_publish_native_bounds() {
    let extensions = plan_only_extensions();
    let handle = NativeOverlayHandle::new();
    bind_native_overlay_handle(&handle, 25, &extensions);
    let entry = Arc::new(
        ExternalOverlay::new(
            25,
            "js-panel",
            FakeComponent::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .with_position(OverlayOptions::default()),
    );

    entry.refresh_for_terminal(80, 24, None);
    assert!(
        entry.bounds().is_some(),
        "the TUI rectangle is still computed for the JS face"
    );
    assert_eq!(
        handle.get_bounds(),
        None,
        "only the native face publishes into the handle"
    );
    // The binding itself is live: a direct publish reaches it.
    assert!(extensions.publish_native_overlay_bounds(
        25,
        NativeOverlayBounds {
            row: 1,
            col: 2,
            width: 3,
            height: 4,
        }
    ));
    assert_eq!(
        handle.get_bounds(),
        Some(NativeOverlayBounds {
            row: 1,
            col: 2,
            width: 3,
            height: 4,
        })
    );
}
