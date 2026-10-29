//! Raw terminal-input hooks (`ctx.ui.onTerminalInput`) across both faces.
//!
//! Covers the producer contract established from upstream pi-tui
//! (`handleTerminalInput` + `ExtensionUIContext.onTerminalInput`):
//!
//! - one canonical raw chunk per physical input (keys, mouse, paste);
//! - native event handlers (`AgentPlugin::on_event("terminal_input")`) and
//!   native `UiService::on_terminal_input` registrations;
//! - the JS face's `ctx.ui.onTerminalInput` list, reached through the
//!   `terminal_input` bridge event;
//! - consume/Pass/replace semantics and the editor gate
//!   (`tui::terminal_input::gate_event_deferred`);
//! - deferred decisions: a slow handler keeps its consume/replace answer
//!   without stalling the loop, and input order is preserved behind it;
//! - fail-open behavior for throwing/timing-out/panicking handlers (including
//!   the deferral backstop and a late answer that must never be applied);
//! - replacement events are never coalesced with the physical input behind
//!   them (the bypass counter would otherwise skip later hook dispatches);
//! - the zero-handler fast path (no dispatch-thread request at all).
//!
//! All fixtures are generated at runtime; nothing is tracked.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde_json::{json, Value};

use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::services::{TerminalInputAction, TerminalInputHandler};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost,
    PluginInfo,
};
use pir::tui::terminal::coalesce_pending_input;
use pir::tui::terminal_input::{
    apply_deferred_action, gate_event_deferred, key_event_data, mouse_event_data, paste_event_data,
    resolve_terminal_input_budget_ms, GateOutcome, TerminalInputPending, TerminalInputRouter,
    TERMINAL_INPUT_BUDGET_ENV_VAR, TERMINAL_INPUT_DEFER_BUDGET,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-terminal-input-{label}-{}-{}",
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

fn extension_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_owned(),
        has_ui: true,
        model: Some("terminal-input-model".to_owned()),
        session_id: Some("terminal-input".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Start a real QuickJS runtime with one seeded extension file.
async fn start_extension(label: &str, source: &str) -> Option<(TempDir, ExtensionHandle)> {
    let temp = TempDir::new(label);
    let path = temp.0.join(format!("{label}.ts"));
    std::fs::write(&path, source).expect("write extension fixture");
    let handle = ExtensionHandle::start(extension_config(&temp.0)).ok()?;
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load extension");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    Some((temp, handle))
}

async fn emit_session_start(handle: &ExtensionHandle) {
    let mut payload = json!({ "reason": "startup" });
    let outcome = handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
}

#[derive(Clone, Copy)]
enum NativeBehavior {
    Pass,
    Consume,
    Replace(&'static str),
    Error,
    Hang,
}

struct NativeTerminalPlugin {
    info: PluginInfo,
    behavior: NativeBehavior,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl NativeTerminalPlugin {
    fn new(behavior: NativeBehavior) -> (Arc<Self>, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                info: PluginInfo::inline("terminal-input-native"),
                behavior,
                seen: Arc::clone(&seen),
            }),
            seen,
        )
    }
}

impl AgentPlugin for NativeTerminalPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["terminal_input".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.payload.clone());
        let behavior = self.behavior;
        Box::pin(async move {
            match behavior {
                NativeBehavior::Pass => PluginEventOutcome::None,
                NativeBehavior::Consume => PluginEventOutcome::TerminalInput {
                    consume: true,
                    data: None,
                },
                NativeBehavior::Replace(data) => PluginEventOutcome::TerminalInput {
                    consume: false,
                    data: Some(data.to_owned()),
                },
                NativeBehavior::Error => PluginEventOutcome::Error {
                    error: "native terminal-input handler failed".to_owned(),
                },
                NativeBehavior::Hang => std::future::pending::<PluginEventOutcome>().await,
            }
        })
    }
}

/// A plugin that does NOT declare terminal-input interest: the fast path must
/// keep it out of the dispatch entirely.
struct UninterestedPlugin {
    info: PluginInfo,
    calls: Arc<AtomicUsize>,
}

impl AgentPlugin for UninterestedPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["session_start".to_owned()])
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { PluginEventOutcome::None })
    }
}

struct RecordingNativeHandler {
    seen: Arc<Mutex<Vec<String>>>,
}

impl TerminalInputHandler for RecordingNativeHandler {
    fn handle(&self, input: &str) -> TerminalInputAction {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(input.to_owned());
        TerminalInputAction::Consume
    }
}

/// A native registry handler that blocks until the test releases it: the TUI's
/// defer backstop must fail the chunk open and keep later chunks out of the
/// still-running request's queue.
struct BlockingNativeHandler {
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl TerminalInputHandler for BlockingNativeHandler {
    fn handle(&self, _input: &str) -> TerminalInputAction {
        let _ = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        TerminalInputAction::Consume
    }
}

/// A native registry handler that answers the FIRST chunk after `delay` with
/// `first_action` and every later chunk with `Pass`, recording the chunks it
/// saw. The delay exceeds the router's inline budget, so the first chunk is
/// deferred and its answer lands on the deferred path.
struct DelayedFirstHandler {
    delay: Duration,
    first_action: TerminalInputAction,
    seen: Arc<Mutex<Vec<String>>>,
}

impl TerminalInputHandler for DelayedFirstHandler {
    fn handle(&self, input: &str) -> TerminalInputAction {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.push(input.to_owned());
        let first = seen.len() == 1;
        drop(seen);
        if !first {
            return TerminalInputAction::Pass;
        }
        std::thread::sleep(self.delay);
        self.first_action.clone()
    }
}

/// A native registry handler that panics on its first chunk and consumes every
/// later one: the dispatcher must isolate the panic (that chunk fails open),
/// survive, and keep dispatching later chunks.
struct PanicOnceHandler {
    calls: AtomicUsize,
    seen: Arc<Mutex<Vec<String>>>,
}

impl TerminalInputHandler for PanicOnceHandler {
    fn handle(&self, input: &str) -> TerminalInputAction {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(input.to_owned());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("terminal-input handler boom (expected: the dispatcher isolates it)");
        }
        TerminalInputAction::Consume
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn bang() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT)
}

/// Run the production gate over one physical input the way the TUI loop does,
/// with a mock "editor" that types plain characters. Returns what the editor
/// received (empty = consumed or replaced before the editor).
fn type_through_gate(router: &TerminalInputRouter, event: Event) -> String {
    type_events_through_gate(router, vec![event])
}

/// The deferred-decision loop from `tui::terminal.rs`: an event whose hook
/// answer misses the inline budget is held (later events never overtake it),
/// the answer is applied when it arrives, and a miss past the defer backstop
/// fails open. The 1 ms poll sleep stands in for the loop's frame tick.
fn type_events_through_gate(router: &TerminalInputRouter, events: Vec<Event>) -> String {
    let mut pending: VecDeque<Event> = events.into();
    let mut bypass = 0usize;
    let mut editor = String::new();
    let mut deferred: Option<(Event, TerminalInputPending)> = None;
    loop {
        // The production loop coalesces a rapid typed burst before it gates
        // the queue front. Replacement events are exempt (`bypass > 0`), so
        // this step runs before the deferred slot is polled, exactly as in
        // `tui::terminal.rs`.
        coalesce_pending_input(&mut pending, bypass);
        let mut current: Option<Event> = None;
        if let Some((held, mut decision)) = deferred.take() {
            match decision.try_take() {
                Some(action) => {
                    if !apply_deferred_action(action, &mut bypass, &mut pending) {
                        current = Some(held);
                    }
                }
                None if Instant::now() >= decision.deadline() => {
                    current = Some(held);
                }
                None => {
                    deferred = Some((held, decision));
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            }
        }
        if current.is_none() && deferred.is_none() {
            let next = match pending.pop_front() {
                Some(next) => next,
                None => break,
            };
            match gate_event_deferred(router, &mut bypass, &next, &mut pending) {
                GateOutcome::Pass => current = Some(next),
                GateOutcome::Handled => {}
                GateOutcome::Deferred(decision) => {
                    deferred = Some((next, decision));
                    continue;
                }
            }
        }
        match current {
            Some(Event::Key(key)) => match key.code {
                KeyCode::Char(c) => editor.push(c),
                KeyCode::Enter => editor.push('\n'),
                _ => {}
            },
            Some(Event::Paste(text)) => editor.push_str(&text),
            _ => {}
        }
    }
    editor
}

// ---------------------------------------------------------------------------
// Encoding parity
// ---------------------------------------------------------------------------

#[test]
fn raw_chunk_encoding_matches_upstream_shapes() {
    assert_eq!(
        key_event_data(&bang()).as_deref(),
        Some("!"),
        "plain character + SHIFT is the character itself"
    );
    assert_eq!(key_event_data(&key(KeyCode::Up)).as_deref(), Some("\x1b[A"));
    assert_eq!(
        key_event_data(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)).as_deref(),
        Some("\x03")
    );
    assert_eq!(
        key_event_data(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)).as_deref(),
        Some("\x1bx")
    );
    assert_eq!(paste_event_data("hi"), "\x1b[200~hi\x1b[201~");
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 2,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(mouse_event_data(&mouse), "\x1b[<0;5;3M");
}

#[test]
fn enter_encoding_matches_upstream_shift_enter_rules() {
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(key_event_data(&enter).as_deref(), Some("\r"));
    // Native Shift+Enter and CSI-u terminals both spell the chord
    // `ESC [ 13 ; 2 u` (upstream `terminal.ts:17`).
    assert_eq!(
        key_event_data(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)).as_deref(),
        Some("\x1b[13;2u")
    );
    // Alt+Enter keeps the legacy ESC-prefixed CR (upstream editor's newline
    // spelling `\x1b\r`), and Alt wins over Shift.
    assert_eq!(
        key_event_data(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)).as_deref(),
        Some("\x1b\r")
    );
    assert_eq!(
        key_event_data(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT | KeyModifiers::SHIFT
        ))
        .as_deref(),
        Some("\x1b\r")
    );
    // Ctrl+Enter has no distinct upstream chord: it stays a bare CR.
    assert_eq!(
        key_event_data(&KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)).as_deref(),
        Some("\r")
    );
    // A key release is not input.
    assert_eq!(
        key_event_data(&KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            crossterm::event::KeyEventKind::Release,
        )),
        None
    );
}

// ---------------------------------------------------------------------------
// Deferral budget
// ---------------------------------------------------------------------------

#[test]
fn terminal_input_budget_defaults_and_env_override() {
    let default_ms = u64::try_from(TERMINAL_INPUT_DEFER_BUDGET.as_millis()).unwrap();
    assert_eq!(default_ms, 5_000, "production default is the host JS cap");
    assert_eq!(resolve_terminal_input_budget_ms(|_| None), default_ms);
    let overridden = |value: &'static str| {
        move |key: &str| (key == TERMINAL_INPUT_BUDGET_ENV_VAR).then(|| value.to_owned())
    };
    assert_eq!(resolve_terminal_input_budget_ms(overridden("250")), 250);
    assert_eq!(
        resolve_terminal_input_budget_ms(overridden(" 250 ")),
        250,
        "surrounding whitespace is trimmed"
    );
    assert_eq!(
        resolve_terminal_input_budget_ms(overridden("0")),
        default_ms,
        "zero disables nothing silently; it keeps the default"
    );
    assert_eq!(
        resolve_terminal_input_budget_ms(overridden("abc")),
        default_ms
    );
    assert_eq!(
        resolve_terminal_input_budget_ms(overridden("-5")),
        default_ms
    );
    assert_eq!(
        resolve_terminal_input_budget_ms(overridden("60001")),
        60_000,
        "the override is capped at one minute"
    );
    assert_eq!(
        resolve_terminal_input_budget_ms(overridden("18446744073709551616")),
        60_000,
        "a value beyond u64 still lands on the one-minute cap, not the default"
    );
}

// ---------------------------------------------------------------------------
// Native event face
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_handler_consumes_key_and_editor_never_sees_it() {
    let (plugin, seen) = NativeTerminalPlugin::new(NativeBehavior::Consume);
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    let editor = type_through_gate(&router, Event::Key(bang()));

    assert_eq!(editor, "", "a consumed key must not reach the editor");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![json!({ "data": "!" })],
        "the native face sees the canonical raw chunk once"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_non_consume_handler_lets_typing_through() {
    let (plugin, seen) = NativeTerminalPlugin::new(NativeBehavior::Pass);
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    assert_eq!(*seen.lock().unwrap(), vec![json!({ "data": "!" })]);
}

#[tokio::test(flavor = "current_thread")]
async fn native_replacement_rewrites_input_without_a_second_dispatch() {
    let (plugin, seen) = NativeTerminalPlugin::new(NativeBehavior::Replace("bang"));
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "bang");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the replacement's decoded keys must not be dispatched to the hook again"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn throwing_native_handler_falls_through_to_the_editor() {
    let (plugin, seen) = NativeTerminalPlugin::new(NativeBehavior::Error);
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn timing_out_handler_falls_through_to_the_editor() {
    let (plugin, _seen) = NativeTerminalPlugin::new(NativeBehavior::Hang);
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router = TerminalInputRouter::with_budgets(
        host,
        Duration::from_millis(200),
        Duration::from_millis(20),
    );

    let started = std::time::Instant::now();
    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a hanging handler must not block the input path beyond its budget"
    );
}

// ---------------------------------------------------------------------------
// Native UiService registry face
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_ui_registry_consumes_and_unsubscribe_restores_typing() {
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscription = host.on_terminal_input(Arc::new(RecordingNativeHandler {
        seen: Arc::clone(&seen),
    }));
    let router = TerminalInputRouter::with_budgets(
        Arc::clone(&host),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "");
    assert_eq!(*seen.lock().unwrap(), vec!["!".to_owned()]);

    subscription.cancel();
    subscription.cancel(); // idempotent
    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Zero-handler fast path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn no_handlers_is_a_zero_dispatch_fast_path() {
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin = Arc::new(UninterestedPlugin {
        info: PluginInfo::inline("terminal-input-uninterested"),
        calls: Arc::clone(&calls),
    });
    let host = Arc::new(PluginHost::build(
        None,
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(router.submitted_requests(), 0);
    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    assert_eq!(
        router.submitted_requests(),
        0,
        "no handlers means the dispatch thread is never asked"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(type_through_gate(&router, Event::Key(key(KeyCode::Up))), "");
}

// ---------------------------------------------------------------------------
// JS face
// ---------------------------------------------------------------------------

fn js_extension(body: &str) -> String {
    format!(
        r#"
        export default function (pi) {{
            pi.on("session_start", (event, ctx) => {{
                {body}
            }});
        }}
        "#
    )
}

#[tokio::test(flavor = "current_thread")]
async fn js_handler_consumes_key_and_editor_never_sees_it() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput((data) => {
            globalThis.__seen = (globalThis.__seen ?? "") + data;
            if (data === "!") return { consume: true };
            return undefined;
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-consume", &source).await else {
        return;
    };
    emit_session_start(&handle).await;
    assert!(handle.has_terminal_input_handlers());
    assert!(
        handle.has_handlers("terminal_input"),
        "the fast-path count and the handler table move together"
    );

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "");
    let seen: Value = handle
        .eval_json("JSON.stringify(globalThis.__seen ?? null)")
        .await
        .expect("eval");
    assert_eq!(seen, json!("!"));
}

/// Upstream calls `onTerminalInput` listeners synchronously; an async
/// handler's Promise has no `consume`/`data` fields and is ignored. The JS
/// face must not wait for (or honor) a later resolution.
#[tokio::test(flavor = "current_thread")]
async fn an_async_terminal_input_handler_is_ignored_like_upstream() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput(async () => {
            await new Promise((resolve) => setTimeout(resolve, 250));
            return { consume: true };
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-async-ignore", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let started = std::time::Instant::now();
    let action = host.dispatch_terminal_input("!").await;
    assert_eq!(
        action,
        TerminalInputAction::Pass,
        "a Promise is not a consume decision"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the host must not block on the unresolved Promise"
    );
}

/// The TUI layer must honor the same slow answer: the 5 ms inline wait misses,
/// the chunk is deferred instead of abandoned, and the handler's consume keeps
/// the key out of the editor.
#[tokio::test(flavor = "current_thread")]
async fn a_slow_sync_js_handler_is_deferred_until_its_consume_answer() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput(() => {
            globalThis.__count = (globalThis.__count ?? 0) + 1;
            const end = Date.now() + 120;
            while (Date.now() < end) {}
            return { consume: true };
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-tui-slow-consume", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_millis(5), Duration::from_secs(2));

    let started = Instant::now();
    let editor = type_through_gate(&router, Event::Key(bang()));
    assert_eq!(
        editor, "",
        "a slow handler's consume decision must not be abandoned at 5 ms"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the chunk waited for the deferred answer"
    );
    assert_eq!(
        router.submitted_requests(),
        1,
        "one dispatch for one physical input"
    );
    let count: Value = handle
        .eval_json("JSON.stringify(globalThis.__count ?? 0)")
        .await
        .expect("eval");
    assert_eq!(count, json!(1));
}

/// While one chunk is deferred, later physical inputs queue behind it (never
/// overtake it) and each is dispatched exactly once; the consumed chunk is not
/// inserted and the following keys keep their order.
#[tokio::test(flavor = "current_thread")]
async fn a_deferred_chunk_holds_input_order_and_dispatches_each_event_once() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput((data) => {
            globalThis.__seen = (globalThis.__seen ?? "") + data;
            const end = Date.now() + 60;
            while (Date.now() < end) {}
            return data === "!" ? { consume: true } : undefined;
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-tui-deferred-order", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_millis(5), Duration::from_secs(2));

    let editor = type_events_through_gate(
        &router,
        vec![
            Event::Key(bang()),
            Event::Key(key(KeyCode::Char('a'))),
            Event::Key(key(KeyCode::Char('b'))),
        ],
    );
    assert_eq!(
        editor, "ab",
        "the consumed chunk is not inserted and the later keys keep their order"
    );
    let seen: Value = handle
        .eval_json("JSON.stringify(globalThis.__seen ?? null)")
        .await
        .expect("eval");
    assert_eq!(seen, json!("!ab"), "the hook sees physical input order");
    assert_eq!(
        router.submitted_requests(),
        3,
        "one dispatch per physical input"
    );
}

/// A deferred replacement lands at the held chunk's position: its decoded
/// keys take the held chunk's place, the physical keys queued behind it keep
/// their order, and the hook still sees every physical chunk exactly once.
#[tokio::test(flavor = "current_thread")]
async fn a_deferred_replacement_is_applied_at_the_held_position() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    let subscription = host.on_terminal_input(Arc::new(DelayedFirstHandler {
        delay: Duration::from_millis(40),
        first_action: TerminalInputAction::Replace("Xy".to_owned()),
        seen: Arc::clone(&seen),
    }));
    let router = TerminalInputRouter::with_budgets(
        Arc::clone(&host),
        Duration::from_millis(5),
        Duration::from_secs(2),
    );

    let editor = type_events_through_gate(
        &router,
        vec![
            Event::Key(bang()),
            Event::Key(key(KeyCode::Char('a'))),
            Event::Key(key(KeyCode::Char('b'))),
        ],
    );
    assert_eq!(
        editor, "Xyab",
        "the replacement takes the held chunk's place and later keys follow it"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["!".to_owned(), "a".to_owned(), "b".to_owned()],
        "each physical chunk is dispatched once, in arrival order"
    );
    assert_eq!(
        router.submitted_requests(),
        3,
        "the replacement's decoded keys are not dispatched to the hook"
    );
    subscription.cancel();
}

/// Coalescing must not merge a replacement's decoded keys with the physical
/// keys behind them. One coalesced `Event::Paste` would consume several
/// bypass reservations with a single pop, leaving the counter positive and
/// silently skipping the hook for later physical inputs. `x\ry` decodes to
/// three key events including a line break, so the paste-run threshold does
/// not matter here.
#[tokio::test(flavor = "current_thread")]
async fn a_deferred_replacement_is_not_coalesced_with_later_physical_keys() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    let subscription = host.on_terminal_input(Arc::new(DelayedFirstHandler {
        delay: Duration::from_millis(40),
        first_action: TerminalInputAction::Replace("x\ry".to_owned()),
        seen: Arc::clone(&seen),
    }));
    let router = TerminalInputRouter::with_budgets(
        Arc::clone(&host),
        Duration::from_millis(5),
        Duration::from_secs(2),
    );

    let editor = type_events_through_gate(
        &router,
        vec![Event::Key(bang()), Event::Key(key(KeyCode::Char('z')))],
    );
    assert_eq!(editor, "x\nyz");
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["!".to_owned(), "z".to_owned()],
        "the physical key behind the replacement must still reach the hook"
    );
    assert_eq!(
        router.submitted_requests(),
        2,
        "one dispatch per physical input; the replacement drains through bypass"
    );
    subscription.cancel();
}

/// The deferral backstop keeps the old fail-open semantics: past the budget
/// the held chunk is processed by the built-in path, its late consume is
/// dropped, and the next chunk does not queue behind the still-running request.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_past_the_defer_backstop_fails_open_without_stalling_the_next_key() {
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    let subscription = host.on_terminal_input(Arc::new(BlockingNativeHandler {
        release: Mutex::new(release_rx),
    }));
    let router = TerminalInputRouter::with_all_budgets(
        Arc::clone(&host),
        Duration::from_millis(5),
        Duration::from_secs(2),
        Duration::from_millis(30),
    );

    let started = Instant::now();
    assert_eq!(
        type_through_gate(&router, Event::Key(bang())),
        "!",
        "the expired chunk fails open to the built-in path"
    );
    assert!(started.elapsed() >= Duration::from_millis(25));
    assert!(
        router.is_busy(),
        "the wedged request is still on the dispatch thread"
    );

    let next_started = Instant::now();
    assert_eq!(
        type_through_gate(&router, Event::Key(key(KeyCode::Char('a')))),
        "a",
        "the next key proceeds while the wedged request is still running"
    );
    assert!(
        next_started.elapsed() < Duration::from_millis(150),
        "the second key must not wait for the wedged handler"
    );
    assert_eq!(
        router.submitted_requests(),
        1,
        "the busy guard skips the hook instead of queueing the second key"
    );

    // Release the handler: its late consume must never reach the built-in
    // path, which already handled both chunks.
    release_tx.send(()).expect("release the handler");
    let settled_by = Instant::now() + Duration::from_secs(2);
    while router.is_busy() && Instant::now() < settled_by {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!router.is_busy(), "the wedged request settled");
    subscription.cancel();
}

/// After the backstop the abandoned request's late answer must be dropped
/// completely: it must neither reach the built-in path retroactively nor hold
/// the busy gate, so the next chunk reaches the hook fresh.
#[tokio::test(flavor = "current_thread")]
async fn a_late_replacement_after_the_backstop_is_dropped() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    let subscription = host.on_terminal_input(Arc::new(DelayedFirstHandler {
        delay: Duration::from_millis(40),
        first_action: TerminalInputAction::Replace("ZZZ".to_owned()),
        seen: Arc::clone(&seen),
    }));
    let router = TerminalInputRouter::with_all_budgets(
        Arc::clone(&host),
        Duration::from_millis(5),
        Duration::from_secs(2),
        Duration::from_millis(20),
    );

    assert_eq!(
        type_through_gate(&router, Event::Key(bang())),
        "!",
        "the expired chunk fails open before the replacement answer arrives"
    );

    // Let the late answer land on the abandoned receiver and release the gate.
    let settled_by = Instant::now() + Duration::from_secs(2);
    while router.is_busy() && Instant::now() < settled_by {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!router.is_busy(), "the abandoned request settled");
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["!".to_owned()],
        "the late replacement was never applied to another chunk"
    );

    // The next physical input is dispatched fresh (the hook answers Pass), so
    // no leaked replacement text and no leaked bypass reservation.
    assert_eq!(
        type_through_gate(&router, Event::Key(key(KeyCode::Char('b')))),
        "b"
    );
    assert_eq!(*seen.lock().unwrap(), vec!["!".to_owned(), "b".to_owned()]);
    assert_eq!(router.submitted_requests(), 2);
    subscription.cancel();
}

/// A panicking handler is isolated on the dispatch thread: that chunk fails
/// open, the busy gate is released, and later chunks still reach the hook.
/// (The expected "handler boom" panic line on stderr is the isolation working.)
#[tokio::test(flavor = "current_thread")]
async fn a_panicking_handler_fails_open_and_the_dispatcher_survives() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(PluginHost::build(None, Vec::new()));
    let subscription = host.on_terminal_input(Arc::new(PanicOnceHandler {
        calls: AtomicUsize::new(0),
        seen: Arc::clone(&seen),
    }));
    let router = TerminalInputRouter::with_budgets(
        Arc::clone(&host),
        Duration::from_millis(50),
        Duration::from_secs(2),
    );

    assert_eq!(
        type_through_gate(&router, Event::Key(bang())),
        "!",
        "a panicking handler fails its chunk open"
    );
    let settled_by = Instant::now() + Duration::from_secs(2);
    while router.is_busy() && Instant::now() < settled_by {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!router.is_busy(), "the panic did not leak the busy gate");
    assert_eq!(
        type_through_gate(&router, Event::Key(key(KeyCode::Char('a')))),
        "",
        "the dispatcher survived and the next chunk is still consumed"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["!".to_owned(), "a".to_owned()],
        "the dispatcher kept dispatching after the panic"
    );
    subscription.cancel();
}

#[tokio::test(flavor = "current_thread")]
async fn js_non_consume_handler_lets_typing_through() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput((data) => {
            globalThis.__seen = (globalThis.__seen ?? "") + data;
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-pass", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    let seen: Value = handle
        .eval_json("JSON.stringify(globalThis.__seen ?? null)")
        .await
        .expect("eval");
    assert_eq!(seen, json!("!"));
}

#[tokio::test(flavor = "current_thread")]
async fn js_handler_replaces_input_and_runs_once() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput((data) => {
            globalThis.__count = (globalThis.__count ?? 0) + 1;
            if (data === "!") return { data: "bang" };
            return undefined;
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-replace", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "bang");
    let count: Value = handle
        .eval_json("JSON.stringify(globalThis.__count ?? 0)")
        .await
        .expect("eval");
    assert_eq!(count, json!(1), "one dispatch per physical input");
}

#[tokio::test(flavor = "current_thread")]
async fn throwing_js_handler_falls_through_to_the_editor() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput(() => {
            throw new Error("js terminal-input boom");
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-throw", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
}

// ---------------------------------------------------------------------------
// Both faces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn one_physical_input_is_dispatched_exactly_once_per_face() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput((data) => {
            globalThis.__js_count = (globalThis.__js_count ?? 0) + 1;
            globalThis.__js_data = data;
            return undefined;
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-and-native", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let (plugin, native_seen) = NativeTerminalPlugin::new(NativeBehavior::Pass);
    let host = Arc::new(PluginHost::build(
        Some(handle.clone()),
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "!");
    assert_eq!(
        native_seen.lock().unwrap().clone(),
        vec![json!({ "data": "!" })],
        "native face: exactly one event"
    );
    let js: Value = handle
        .eval_json(
            "JSON.stringify({ count: globalThis.__js_count ?? 0, data: globalThis.__js_data ?? null })",
        )
        .await
        .expect("eval");
    assert_eq!(
        js,
        json!({ "count": 1, "data": "!" }),
        "JS face: exactly one event with the same payload"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumption_stops_the_js_face_from_seeing_the_chunk() {
    let source = js_extension(
        r#"
        ctx.ui.onTerminalInput((data) => {
            globalThis.__js_count = (globalThis.__js_count ?? 0) + 1;
        });
        "#,
    );
    let Some((_temp, handle)) = start_extension("js-after-native-consume", &source).await else {
        return;
    };
    emit_session_start(&handle).await;

    let (plugin, _seen) = NativeTerminalPlugin::new(NativeBehavior::Consume);
    let host = Arc::new(PluginHost::build(
        Some(handle.clone()),
        vec![plugin as Arc<dyn AgentPlugin>],
    ));
    let router =
        TerminalInputRouter::with_budgets(host, Duration::from_secs(2), Duration::from_secs(2));

    assert_eq!(type_through_gate(&router, Event::Key(bang())), "");
    let count: Value = handle
        .eval_json("JSON.stringify(globalThis.__js_count ?? 0)")
        .await
        .expect("eval");
    assert_eq!(
        count,
        json!(0),
        "upstream consumption short-circuit: later listeners never run"
    );
}
