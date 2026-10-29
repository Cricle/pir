//! `ctx.ui.custom` / external overlay slot (lane W13-O).
//!
//! Drives the pure containers, no terminal: the generic external-overlay slot
//! that `ctx.ui.custom` (JS and native faces) exercises. The invariants under
//! test are the ones the one shared overlay mechanism promises — a custom
//! overlay takes the focus like any selector, consumes keys and pastes instead
//! of the editor, is bounded and promoted like the other dialogs, is cleared
//! by the session-reset funnel, and degrades (never wedges, never panics) when
//! a component fails.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::plugins::plugin::services::{FactoryRenderContext, OverlayComponent};
use pir::tui::terminal::{
    external_overlay_key_route, external_overlay_paste_route, paste_route, render_active_overlay,
    ExternalOverlay, ExternalOverlayKey, ExternalOverlaySlot, ExternalPasteRoute, Overlay,
    PasteRoute, MAX_QUEUED_DIALOGS,
};

/// One controllable component: records the input chunks it received and
/// renders a fixed frame carrying its id.
struct FakeComponent {
    label: String,
    inputs: Mutex<Vec<String>>,
    renders: AtomicUsize,
    dispose_calls: AtomicUsize,
}

impl FakeComponent {
    fn new(label: &str) -> Arc<Self> {
        Arc::new(Self {
            label: label.to_string(),
            inputs: Mutex::new(Vec::new()),
            renders: AtomicUsize::new(0),
            dispose_calls: AtomicUsize::new(0),
        })
    }

    fn inputs(&self) -> Vec<String> {
        self.inputs.lock().unwrap().clone()
    }
}

impl OverlayComponent for FakeComponent {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        self.renders.fetch_add(1, Ordering::SeqCst);
        vec![format!("{} @{}", self.label, context.width)]
    }

    fn handle_input(&self, data: &str) {
        self.inputs.lock().unwrap().push(data.to_string());
    }

    fn dispose(&self) {
        self.dispose_calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// A component whose render and input both panic (a third-party component
/// bug): the host must isolate it, not unwind the frame or the input loop.
struct PanickingComponent;

impl OverlayComponent for PanickingComponent {
    fn render(&self, _context: &FactoryRenderContext<'_>) -> Vec<String> {
        panic!("component render failed");
    }

    fn handle_input(&self, _data: &str) {
        panic!("component input failed");
    }
}

fn overlay_with(label: &str, deadline: Instant) -> (Arc<ExternalOverlay>, Arc<FakeComponent>) {
    let component = FakeComponent::new(label);
    let erased: Arc<dyn OverlayComponent> = component.clone();
    (
        Arc::new(ExternalOverlay::new(1, label, erased, deadline)),
        component,
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

// ---------------------------------------------------------------------------
// (a) a custom overlay takes the focus
// ---------------------------------------------------------------------------

#[test]
fn custom_overlay_makes_any_open_true_and_paints() {
    let now = Instant::now();
    let mut overlay = Overlay::default();
    assert!(!overlay.any_open(), "nothing is open at rest");

    let (entry, component) = overlay_with("panel", now + Duration::from_secs(60));
    let admitted = matches!(overlay.external_mut().admit(entry, now), Ok(true));
    assert!(admitted, "an idle slot installs the overlay directly");

    assert!(
        overlay.any_open(),
        "the external slot participates in the same focus predicate as the typed selectors"
    );
    assert!(overlay.external().is_open());

    // The dock's editor-slot painter serves the external overlay first.
    let visible = overlay.external().visible().expect("visible");
    assert!(
        visible.needs_render(80),
        "a fresh overlay always needs a frame"
    );
    visible.refresh(80, None);
    let lines = render_active_overlay(&overlay, 80).expect("the overlay replaces the editor slot");
    assert_eq!(lines, vec!["panel @80".to_string()]);
    assert_eq!(component.renders.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// (b) key input routes to the overlay, not the editor
// ---------------------------------------------------------------------------

#[test]
fn keys_and_pastes_route_to_the_custom_overlay() {
    let now = Instant::now();
    let mut overlay = Overlay::default();
    let (entry, component) = overlay_with("panel", now + Duration::from_secs(60));
    assert!(matches!(overlay.external_mut().admit(entry, now), Ok(true)));

    // A printable key is delivered to the component as the canonical chunk
    // upstream's focused component receives — never to the main editor.
    let route = external_overlay_key_route(overlay.external(), &key(KeyCode::Char('j')))
        .expect("an open overlay consumes every key");
    assert_eq!(route, ExternalOverlayKey::Input("j".to_string()));
    overlay.external().visible().unwrap().handle_input("j");

    let route = external_overlay_key_route(overlay.external(), &key(KeyCode::Enter))
        .expect("Enter is routed");
    assert_eq!(route, ExternalOverlayKey::Input("\r".to_string()));
    overlay.external().visible().unwrap().handle_input("\r");

    // Arrow keys carry their CSI spelling (components match raw sequences).
    let route = external_overlay_key_route(overlay.external(), &key(KeyCode::Down)).unwrap();
    assert_eq!(route, ExternalOverlayKey::Input("\u{1b}[B".to_string()));

    assert_eq!(component.inputs(), vec!["j".to_string(), "\r".to_string()]);

    // Esc is the component's key (upstream examples use it for "go back"), so
    // it is forwarded rather than swallowed by the host.
    let route = external_overlay_key_route(overlay.external(), &key(KeyCode::Esc)).unwrap();
    assert_eq!(route, ExternalOverlayKey::Input("\u{1b}".to_string()));

    // The dialog family's cancel chord is the host's guaranteed way out.
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(
        external_overlay_key_route(overlay.external(), &ctrl_c),
        Some(ExternalOverlayKey::Dismiss)
    );

    // A paste goes nowhere near the main editor while the modal owns focus:
    // this is the exact predicate the loop's `Event::Paste` arm feeds.
    assert_eq!(
        paste_route(false, false, overlay.any_open()),
        PasteRoute::Swallowed
    );
}

#[test]
fn a_route_with_no_overlay_is_none() {
    let overlay = Overlay::default();
    assert_eq!(
        external_overlay_key_route(overlay.external(), &key(KeyCode::Char('x'))),
        None,
        "an empty slot routes nothing (the editor keeps the key)"
    );
}

/// Upstream `OverlayHandle.setHidden` (tui.ts:309-321): a hidden overlay
/// stops painting and releases the keyboard/paste focus to the editor while
/// keeping its slot (so `done()`/the deadline still settle it).
#[test]
fn a_hidden_overlay_releases_focus_without_leaving_its_slot() {
    let now = Instant::now();
    let mut overlay = Overlay::default();
    let (entry, component) = overlay_with("panel", now + Duration::from_secs(60));
    assert!(matches!(
        overlay.external_mut().admit(Arc::clone(&entry), now),
        Ok(true)
    ));
    assert!(overlay.any_open());

    entry.set_hidden(true);
    assert!(entry.is_hidden());
    assert!(
        !overlay.any_open(),
        "a hidden overlay does not own the focus"
    );
    assert_eq!(
        external_overlay_key_route(overlay.external(), &key(KeyCode::Char('j'))),
        None,
        "keys go to the editor while hidden"
    );
    assert_eq!(
        external_overlay_paste_route(overlay.external(), "hello"),
        ExternalPasteRoute::None
    );
    assert_eq!(
        paste_route(false, false, overlay.any_open()),
        PasteRoute::Editor
    );
    assert!(
        render_active_overlay(&overlay, 80).is_none(),
        "a hidden overlay paints nothing (the editor slot returns)"
    );
    assert_eq!(
        component.renders.load(Ordering::SeqCst),
        0,
        "no component render runs while hidden"
    );

    // Showing it again restores the single overlay focus path.
    entry.set_hidden(false);
    assert!(overlay.any_open());
    assert_eq!(
        external_overlay_key_route(overlay.external(), &key(KeyCode::Char('j'))),
        Some(ExternalOverlayKey::Input("j".to_string()))
    );
    let settled = overlay
        .external_mut()
        .settle_key("panel")
        .expect("the hidden-then-shown overlay still settles");
    settled.dispose();
    assert!(!overlay.external().is_open());
}

#[test]
fn a_paste_reaches_the_component_not_the_editor() {
    let now = Instant::now();
    let mut overlay = Overlay::default();
    let (entry, component) = overlay_with("panel", now + Duration::from_secs(60));
    assert!(matches!(overlay.external_mut().admit(entry, now), Ok(true)));

    // The paste is delivered as the canonical bracketed chunk upstream's
    // focused component parses, and it never reaches the main editor.
    let route = external_overlay_paste_route(overlay.external(), "hello");
    let ExternalPasteRoute::Component(data) = route else {
        panic!("an open overlay owns the paste");
    };
    assert!(
        data.contains("hello"),
        "the pasted text is carried in the chunk: {data:?}"
    );
    overlay.external().visible().unwrap().handle_input(&data);
    assert_eq!(component.inputs(), vec![data]);

    // A paste still routes to the editor when nothing is open.
    let empty = Overlay::default();
    assert_eq!(
        external_overlay_paste_route(empty.external(), "hello"),
        ExternalPasteRoute::None
    );
    assert_eq!(
        paste_route(false, false, empty.any_open()),
        PasteRoute::Editor
    );
}

// ---------------------------------------------------------------------------
// (c) dismissal clears the slot: no leak across two consecutive opens
// ---------------------------------------------------------------------------

#[test]
fn dismissal_clears_the_slot_across_two_opens() {
    let now = Instant::now();
    let mut slot = ExternalOverlaySlot::default();

    for round in 0..2 {
        let (entry, component) =
            overlay_with(&format!("round-{round}"), now + Duration::from_secs(60));
        assert!(matches!(slot.admit(Arc::clone(&entry), now), Ok(true)));
        assert!(slot.is_open(), "round {round}: the overlay is visible");

        let settled = slot.settle(|_| true).expect("the visible overlay settles");
        assert!(settled.id == entry.id);
        settled.dispose();
        assert_eq!(
            component.dispose_calls.load(Ordering::SeqCst),
            1,
            "round {round}: dispose runs exactly once"
        );

        assert!(!slot.is_open(), "round {round}: nothing leaks");
        assert_eq!(slot.queue_len(), 0, "round {round}: the queue is empty");
    }
}

// ---------------------------------------------------------------------------
// (d) failures degrade without wedging
// ---------------------------------------------------------------------------

#[test]
fn a_panicking_component_is_isolated() {
    let now = Instant::now();
    let component: Arc<dyn OverlayComponent> = Arc::new(PanickingComponent);
    let entry = Arc::new(ExternalOverlay::new(
        7,
        "boom",
        component,
        now + Duration::from_secs(60),
    ));

    // Render: the panic is caught and the overlay keeps an actionable row
    // instead of unwinding the tick (and thus the input loop).
    entry.refresh(40, None);
    let lines = entry.lines();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("Ctrl+C"),
        "the fallback row says how to get out: {lines:?}"
    );

    // Input: the panic is caught too, and the overlay stays dismissable.
    entry.handle_input("x");
    assert!(!entry.expired(now));

    // Dismissal still works after both failures.
    let mut slot = ExternalOverlaySlot::default();
    assert!(matches!(slot.admit(Arc::clone(&entry), now), Ok(true)));
    assert!(
        slot.settle(|_| true).is_some(),
        "the failed overlay still closes"
    );
    assert!(!slot.is_open());
}

#[test]
fn a_request_without_a_component_settles_like_a_dismissal() {
    // `open_custom_overlay` answers this shape with the cancel reply; the
    // slot-level contract is that nothing is installed and no waiter is left.
    let mut slot = ExternalOverlaySlot::default();
    let now = Instant::now();
    let (entry, _) = overlay_with("never", now + Duration::from_secs(60));
    assert!(matches!(slot.admit(entry, now), Ok(true)));
    let settled = slot.settle(|_| true);
    assert!(settled.is_some());
    assert!(!slot.is_open());
}

// ---------------------------------------------------------------------------
// (e) the queue cap and the deadline behave like the other dialogs
// ---------------------------------------------------------------------------

#[test]
fn the_queue_is_capped_like_the_typed_dialogs() {
    let now = Instant::now();
    let mut slot = ExternalOverlaySlot::default();
    let far = now + Duration::from_secs(600);

    let (visible, _) = overlay_with("visible", far);
    assert!(matches!(slot.admit(visible, now), Ok(true)));

    for index in 0..MAX_QUEUED_DIALOGS {
        let (entry, _) = overlay_with(&format!("queued-{index}"), far);
        assert!(
            matches!(slot.admit(entry, now), Ok(false)),
            "queued overlay {index} should wait, not replace the visible one"
        );
    }
    assert_eq!(slot.queue_len(), MAX_QUEUED_DIALOGS);

    // One past the cap is REJECTED (returned for the caller's cancel reply),
    // exactly like `admit_dialog`.
    let (overflow, _) = overlay_with("overflow", far);
    assert!(slot.admit(overflow, now).is_err());
    assert_eq!(slot.queue_len(), MAX_QUEUED_DIALOGS);
    assert_eq!(
        slot.visible().map(|overlay| overlay.key.as_str()),
        Some("visible"),
        "overflow never displaces the visible overlay"
    );
}

#[test]
fn settlement_promotes_the_queue_head() {
    let now = Instant::now();
    let mut slot = ExternalOverlaySlot::default();
    let far = now + Duration::from_secs(600);

    let (first, _) = overlay_with("first", far);
    let (second, _) = overlay_with("second", far);
    assert!(matches!(slot.admit(first, now), Ok(true)));
    assert!(matches!(slot.admit(second, now), Ok(false)));
    assert_eq!(slot.visible().unwrap().key, "first");

    // The single funnel: take the visible entry AND promote the head.
    let settled = slot.settle(|_| true).expect("first settles");
    assert_eq!(settled.key, "first");
    assert_eq!(
        slot.visible().map(|overlay| overlay.key.as_str()),
        Some("second"),
        "the queued overlay takes the slot in the same step"
    );
    assert_eq!(slot.queue_len(), 0);
}

#[test]
fn the_deadline_dismisses_like_a_timed_out_dialog() {
    // A request whose deadline already passed never becomes visible.
    let now = Instant::now();
    let mut slot = ExternalOverlaySlot::default();
    let (expired, _) = overlay_with("expired", now - Duration::from_millis(1));
    assert!(matches!(slot.admit(expired, now), Ok(true)));
    let settled = slot
        .sweep_expired(now)
        .expect("an expired overlay is swept on the next tick");
    assert_eq!(settled.key, "expired");
    assert!(!slot.is_open());

    // A QUEUED overlay past its deadline is dropped before it can surface.
    let mut slot = ExternalOverlaySlot::default();
    let (visible, _) = overlay_with("visible", now + Duration::from_secs(60));
    let (stale, _) = overlay_with("stale", now - Duration::from_millis(1));
    assert!(matches!(slot.admit(visible, now), Ok(true)));
    // It queues behind the visible one (the slot is occupied).
    assert!(matches!(slot.admit(stale, now), Ok(false)));
    // The next admission sweeps the stale entry (no promotion of a dead
    // waiter), and the visible one is untouched.
    let (fresh, _) = overlay_with("fresh", now + Duration::from_secs(60));
    assert!(matches!(slot.admit(fresh, now), Ok(false)));
    assert_eq!(slot.queue_len(), 1);
    assert!(
        slot.find_key("stale").is_none(),
        "the expired queued overlay was swept"
    );
    assert_eq!(slot.visible().unwrap().key, "visible");
}

#[test]
fn session_reset_clears_every_live_overlay() {
    // `clear_dialog_state` drains the visible entry and the queue in one step
    // (the session-switch contract); every entry comes back for its cancel
    // reply + dispose.
    let now = Instant::now();
    let mut slot = ExternalOverlaySlot::default();
    let far = now + Duration::from_secs(600);
    let (visible, visible_component) = overlay_with("visible", far);
    let (queued, _) = overlay_with("queued", far);
    assert!(matches!(slot.admit(visible, now), Ok(true)));
    assert!(matches!(slot.admit(queued, now), Ok(false)));

    let cleared = slot.clear();
    assert_eq!(cleared.len(), 2, "visible + queued both come back");
    assert!(!slot.is_open());
    assert_eq!(slot.queue_len(), 0);
    for entry in &cleared {
        entry.dispose();
    }
    assert_eq!(visible_component.dispose_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_repaint_request_survives_until_the_next_refresh() {
    let now = Instant::now();
    let (entry, _) = overlay_with("panel", now + Duration::from_secs(60));

    assert!(
        entry.needs_render(80),
        "an unrendered overlay needs a frame"
    );
    entry.refresh(80, None);
    assert!(!entry.needs_render(80), "a clean frame is not re-rendered");

    // `tui.requestRender()` from the component: exactly one more frame.
    entry.mark_dirty();
    assert!(entry.needs_render(80));
    entry.refresh(80, None);
    assert!(!entry.needs_render(80));

    // A width change forces a frame regardless of the flag.
    assert!(entry.needs_render(40));
}
