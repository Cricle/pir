//! W10-D TUI renderer-deferral pins (unregistered suite; the foreman
//! registers it in the project's TUI test target).
//!
//! The live-freeze audit (W9-A) found the self-sustaining starvation path:
//! `tui.requestRender()` from a factory widget dirtied EVERY factory slot,
//! and the event loop then invoked the renderers synchronously (JS up to
//! 3.05 s each, native factories unbounded) while holding the factory
//! registry guards. These pins cover the fix's pure seams:
//!
//! 1. a keyed `tui.requestRender()` dirties only the calling widget;
//! 2. a slow renderer cannot block the scheduler/loop calls;
//! 3. N requests inside the coalescing window produce ONE render (latest
//!    request wins);
//! 4. completed results install in place (registry order preserved) and a
//!    stale token can never overwrite newer lines;
//! 5. the registry lock is released before any render callback runs;
//! 6. a steady widget set performs zero factory calls.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::plugins::plugin::services::{
    FactoryRenderContext, RendererKind as ServiceRendererKind, RendererService,
};
use pir::tui::renderers::{
    collect_factory_widget_render_keys, install_factory_widget_lines, mark_factory_widget_dirty,
    parse_factory_render_request, serve_render_jobs, FactoryRenderOutcome, FactorySlotKey,
    FactoryWidget, OwnedFactoryRendererInvoker, RenderInputs, RenderScheduler, RendererKind,
    ScheduledRender,
};

fn widget(key: &str) -> FactoryWidget {
    FactoryWidget {
        key: key.to_string(),
        placement: "above_transcript".to_string(),
        lines: Vec::new(),
        width: 0,
        theme_generation: 0,
        dirty: true,
    }
}

fn widget_keys(widgets: &[FactoryWidget]) -> Vec<String> {
    widgets.iter().map(|widget| widget.key.clone()).collect()
}

// ---------------------------------------------------------------------------
// 1. Key-scoped requestRender
// ---------------------------------------------------------------------------

#[test]
fn request_render_on_widget_a_does_not_dirty_widget_b() {
    let mut widgets = vec![widget("a"), widget("b"), widget("c")];
    for widget in &mut widgets {
        widget.dirty = false;
    }

    let targets = parse_factory_render_request(Some(r#"{"kind":"widget","key":"a"}"#))
        .expect("widget payload");
    assert_eq!(targets, vec![FactorySlotKey::Widget("a".to_string())]);
    for slot in targets {
        if let FactorySlotKey::Widget(key) = slot {
            assert!(mark_factory_widget_dirty(&mut widgets, &key));
        }
    }
    assert!(widgets[0].dirty, "the calling widget must be dirtied");
    assert!(!widgets[1].dirty, "widget B must stay clean");
    assert!(!widgets[2].dirty, "widget C must stay clean");

    // Footer/header signals target their own slot, not every widget.
    assert_eq!(
        parse_factory_render_request(Some(r#"{"kind":"footer"}"#)),
        Some(vec![FactorySlotKey::Footer])
    );
    assert_eq!(
        parse_factory_render_request(Some(r#"{"kind":"header"}"#)),
        Some(vec![FactorySlotKey::Header])
    );

    // No payload = the legacy broadcast (all slots dirty).
    assert_eq!(parse_factory_render_request(None), None);
    assert_eq!(parse_factory_render_request(Some("not json")), None);
}

// ---------------------------------------------------------------------------
// 2. A slow renderer leaves the loop responsive
// ---------------------------------------------------------------------------

#[test]
fn slow_renderer_leaves_the_scheduler_responsive() {
    let scheduler = Arc::new(RenderScheduler::<FactorySlotKey>::new(
        Duration::from_millis(10),
    ));
    let key = FactorySlotKey::Widget("slow".to_string());
    scheduler.note_dirty(&key);
    let inputs = RenderInputs::factory(80, 3);
    assert!(scheduler.request(
        key.clone(),
        RendererKind::Widget,
        inputs,
        scheduler.signal(&key),
        None,
    ));
    let jobs = scheduler.take_due(Instant::now());
    assert_eq!(jobs.len(), 1);

    let (job_tx, job_rx) = mpsc::sync_channel::<ScheduledRender<FactorySlotKey>>(4);
    let (result_tx, result_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        serve_render_jobs(
            job_rx,
            move |_job| {
                // Stand-in for a slow JS actor / hung native factory.
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test release");
                FactoryRenderOutcome::Lines(vec!["rendered".to_string()])
            },
            move |job, outcome| {
                result_tx.send((job, outcome)).expect("publish");
            },
        );
    });
    job_tx.send(jobs.into_iter().next().unwrap()).unwrap();
    // Let the worker enter the (blocking) render callback.
    std::thread::sleep(Duration::from_millis(30));

    // Every loop-facing scheduler operation must be instant: the renderer
    // runs on the worker, never under the scheduler lock.
    let started = Instant::now();
    assert!(scheduler.has_work());
    assert_eq!(scheduler.queued_len(), 0);
    let _ = scheduler.signal(&key);
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "scheduler calls blocked on the renderer"
    );

    release_tx.send(()).unwrap();
    let (job, outcome) = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let FactoryRenderOutcome::Lines(lines) = outcome else {
        panic!("the fake renderer must produce lines");
    };
    assert_eq!(lines, vec!["rendered".to_string()]);
    assert!(scheduler.accept(&job.key, job.token, job.inputs));
    assert!(!scheduler.has_work());
    drop(job_tx);
    worker.join().unwrap();
}

/// A native widget factory that never answers inside the bounded wait.
struct SlowNativeWidget;

impl RendererService for SlowNativeWidget {
    fn has(&self, kind: ServiceRendererKind, key: &str) -> bool {
        kind == ServiceRendererKind::Widget && key == "slow-native"
    }

    fn render_widget(
        &self,
        _key: &str,
        _context: &FactoryRenderContext<'_>,
    ) -> Option<Vec<String>> {
        std::thread::sleep(Duration::from_secs(2));
        Some(vec!["late".to_string()])
    }
}

#[test]
fn native_factory_timeout_is_bounded_and_keeps_the_cached_lines() {
    let service: Arc<dyn RendererService> = Arc::new(SlowNativeWidget);
    let native = Arc::new(Mutex::new(Some(service)));
    let executor = OwnedFactoryRendererInvoker::new(native, None, None, Arc::new(|| None), None);
    let job = ScheduledRender {
        key: FactorySlotKey::Widget("slow-native".to_string()),
        kind: RendererKind::Widget,
        token: 1,
        inputs: RenderInputs::factory(80, 0),
        data: None,
    };

    let started = Instant::now();
    let outcome = executor.render(&job);
    let elapsed = started.elapsed();
    assert!(
        matches!(outcome, FactoryRenderOutcome::TimedOut),
        "a native factory past the bounded wait must time out"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "the worker must actually wait for the bounded window"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the bounded wait must not inherit the factory's own hang"
    );

    // While the abandoned helper thread still runs, the next request returns
    // immediately instead of spawning one leaked thread per tick.
    let second = Instant::now();
    let outcome = executor.render(&job);
    assert!(matches!(outcome, FactoryRenderOutcome::TimedOut));
    assert!(second.elapsed() < Duration::from_millis(100));

    // A timeout install keeps the previous lines (only the inputs advance).
    let mut widgets = vec![widget("slow-native")];
    widgets[0].lines = vec!["previous".to_string()];
    assert!(!install_factory_widget_lines(
        &mut widgets,
        "slow-native",
        80,
        0,
        None
    ));
    assert_eq!(widgets[0].lines, vec!["previous".to_string()]);
}

// ---------------------------------------------------------------------------
// 3. Coalescing: N signals in the window => 1 render, latest wins
// ---------------------------------------------------------------------------

#[test]
fn requests_inside_the_window_coalesce_to_one_render() {
    let scheduler = RenderScheduler::<FactorySlotKey>::new(Duration::from_millis(250));
    let key = FactorySlotKey::Widget("todo".to_string());
    let t0 = Instant::now();

    for index in 0..5 {
        scheduler.note_dirty(&key);
        let data = Some(Arc::new(serde_json::json!({ "generation": index })));
        assert!(scheduler.request(
            key.clone(),
            RendererKind::Widget,
            RenderInputs::factory(80, 0),
            scheduler.signal(&key),
            data,
        ));
    }
    assert_eq!(
        scheduler.queued_len(),
        1,
        "the latest-wins map holds ONE queued job per key"
    );

    let first = scheduler.take_due(t0);
    assert_eq!(first.len(), 1, "N requests inside the window => ONE render");
    let expected = serde_json::json!({ "generation": 4 });
    assert_eq!(
        first[0].data.as_deref(),
        Some(&expected),
        "the coalesced job carries the LATEST payload"
    );
    let first_token = first[0].token;

    // Inside the window: nothing dispatches.
    scheduler.note_dirty(&key);
    assert!(scheduler.request(
        key.clone(),
        RendererKind::Widget,
        RenderInputs::factory(80, 0),
        scheduler.signal(&key),
        None,
    ));
    assert!(scheduler
        .take_due(t0 + Duration::from_millis(100))
        .is_empty());

    // After the window: exactly one further job with a newer token.
    let second = scheduler.take_due(t0 + Duration::from_millis(300));
    assert_eq!(second.len(), 1);
    assert!(
        second[0].token > first_token,
        "each dispatch gets a fresh generation token"
    );

    // Identical inputs + identical signal are deduped per tick.
    let queued_before = scheduler.queued_len();
    let signal = scheduler.signal(&key);
    assert!(!scheduler.request(
        key.clone(),
        RendererKind::Widget,
        RenderInputs::factory(80, 0),
        signal,
        None,
    ));
    assert_eq!(scheduler.queued_len(), queued_before);
}

// ---------------------------------------------------------------------------
// 4. Results install in place, preserve order, reject stale tokens
// ---------------------------------------------------------------------------

#[test]
fn results_install_in_place_and_preserve_order() {
    let mut widgets = vec![widget("a"), widget("b"), widget("c")];
    for widget in &mut widgets {
        widget.dirty = false;
    }

    assert!(install_factory_widget_lines(
        &mut widgets,
        "b",
        80,
        7,
        Some(vec!["B".to_string()])
    ));
    assert_eq!(widget_keys(&widgets), vec!["a", "b", "c"]);
    assert_eq!(widgets[1].lines, vec!["B".to_string()]);
    assert!(!widgets[1].dirty);
    assert_eq!(widgets[1].width, 80);
    assert_eq!(widgets[1].theme_generation, 7);
    assert!(widgets[0].lines.is_empty(), "no other widget was touched");

    // Native-timeout contract: keep the previous lines, record the inputs.
    assert!(!install_factory_widget_lines(
        &mut widgets,
        "b",
        100,
        8,
        None
    ));
    assert_eq!(widgets[1].lines, vec!["B".to_string()]);
    assert_eq!(widgets[1].width, 100);
    assert_eq!(widgets[1].theme_generation, 8);

    // Unknown identity: nothing changes.
    assert!(!install_factory_widget_lines(
        &mut widgets,
        "missing",
        80,
        0,
        Some(vec!["X".to_string()])
    ));
    assert_eq!(widget_keys(&widgets), vec!["a", "b", "c"]);

    // A stale result (superseded token) is rejected by the scheduler.
    let scheduler = RenderScheduler::<FactorySlotKey>::new(Duration::ZERO);
    let key = FactorySlotKey::Widget("b".to_string());
    let inputs = RenderInputs::factory(80, 7);
    scheduler.note_dirty(&key);
    assert!(scheduler.request(
        key.clone(),
        RendererKind::Widget,
        inputs,
        scheduler.signal(&key),
        None,
    ));
    let stale = scheduler.take_due(Instant::now()).pop().unwrap();
    scheduler.note_dirty(&key);
    assert!(scheduler.request(
        key.clone(),
        RendererKind::Widget,
        inputs,
        scheduler.signal(&key),
        None,
    ));
    let fresh = scheduler
        .take_due(Instant::now() + Duration::from_secs(1))
        .pop()
        .unwrap();
    assert!(
        !scheduler.accept(&key, stale.token, stale.inputs),
        "a superseded render must not install"
    );
    assert!(scheduler.accept(&key, fresh.token, fresh.inputs));
}

// ---------------------------------------------------------------------------
// 5. No registry lock is held across a blocking render callback
// ---------------------------------------------------------------------------

#[test]
fn widget_snapshot_releases_the_lock_before_the_request_callback() {
    let registry = Arc::new(Mutex::new(vec![widget("a")]));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let snapshot_registry = Arc::clone(&registry);
    let handle = std::thread::spawn(move || {
        collect_factory_widget_render_keys(&snapshot_registry, 80, 0, |_key| {
            // Stand-in for `invoker.invoke`: block while the "render" runs.
            entered_tx.send(()).expect("entered");
            release_rx.recv().expect("release");
        });
    });

    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the snapshot must find the dirty widget and run the request callback");
    // The request callback is blocked; the registry guard must be free.
    let guard = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(guard.len(), 1);
    drop(guard);

    release_tx.send(()).unwrap();
    handle.join().unwrap();
}

// ---------------------------------------------------------------------------
// 6. Steady frame => zero factory calls
// ---------------------------------------------------------------------------

#[test]
fn steady_frame_makes_zero_factory_requests() {
    let registry = Arc::new(Mutex::new(vec![widget("steady")]));

    let mut calls = 0;
    collect_factory_widget_render_keys(&registry, 80, 5, |_key| calls += 1);
    assert_eq!(calls, 1, "the dirty widget renders once");

    // Simulate the worker result install (inputs stamped, dirty cleared).
    {
        let mut widgets = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(install_factory_widget_lines(
            &mut widgets,
            "steady",
            80,
            5,
            Some(vec!["line".to_string()])
        ));
    }

    calls = 0;
    collect_factory_widget_render_keys(&registry, 80, 5, |_key| calls += 1);
    assert_eq!(calls, 0, "a steady frame performs ZERO factory calls");

    // A width move is a cache-key move and re-requests exactly once.
    collect_factory_widget_render_keys(&registry, 60, 5, |_key| calls += 1);
    assert_eq!(calls, 1);

    // A theme-generation move does the same.
    calls = 0;
    collect_factory_widget_render_keys(&registry, 60, 6, |_key| calls += 1);
    assert_eq!(calls, 1);
}
