//! Raw terminal-input hooks (`ctx.ui.onTerminalInput`) for the TUI.
//!
//! Upstream (pi-tui `handleTerminalInput` / `ExtensionUIContext
//! .onTerminalInput`, `dist/core/extensions/types.d.ts`) registers raw stdin
//! listeners that receive one complete terminal chunk as a string and return
//! `{ consume?: boolean; data?: string } | undefined`. This module is the pir
//! producer/consumer for that contract:
//!
//! - [`key_event_data`], [`mouse_event_data`] and [`paste_event_data`] encode
//!   crossterm's parsed events back into the canonical terminal chunk the
//!   upstream listener would have received (the TUI keeps crossterm as its
//!   byte parser, so the encode step is where parity is established).
//! - [`TerminalInputRouter`] owns a dedicated dispatch thread. The TUI's input
//!   loop must decide whether a key reaches the editor, but a slow
//!   JS/extension handler must never stall rendering. The router therefore
//!   waits at most [`TERMINAL_INPUT_SYNC_BUDGET`] inline for the host's
//!   answer; a handler that misses that budget does **not** lose the chunk:
//!   [`gate_event_deferred`] returns [`GateOutcome::Deferred`], the loop keeps
//!   rendering, and the answer is applied in input order when it arrives
//!   (bounded by [`TERMINAL_INPUT_DEFER_BUDGET`]). Requests are FIFO and the
//!   queue is bounded: when it is full the chunk falls through immediately.
//! - [`gate_event_deferred`] is the production gate between `pending_input`
//!   and the built-in handling. It serializes the event, dispatches it, and
//!   either passes it through, consumes it, defers the decision, or queues the
//!   replacement's decoded events (marked so they are not dispatched to the
//!   hooks again). Replacement payloads are bounded by
//!   [`TERMINAL_INPUT_REPLACEMENT_MAX_BYTES`] and
//!   [`TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS`]; an over-cap replacement is
//!   consumed (dropped) with a diagnostic, so one handler answer can never
//!   enqueue unbounded work ahead of real typing. [`gate_event`] is the
//!   legacy synchronous wrapper (a late answer fails open).
//!
//! Ordering contract (documented because the dispatch is off-thread):
//! physical inputs are processed by the dispatch thread in arrival order,
//! native registrations first (registration order), then native plugins in the
//! host's ordered list, then the JS face; one `terminal_input` event is
//! dispatched per physical input. A chunk whose answer misses the inline sync
//! budget is held by the input loop until the answer arrives or
//! [`TERMINAL_INPUT_DEFER_BUDGET`] elapses; later physical inputs are queued
//! behind it, never processed ahead of it, so a late consume/replace still
//! applies to the chunk it was asked about and never to a later one. After the
//! defer budget the chunk fails open (its late answer is dropped, never applied
//! retroactively). This is a deliberate deviation from upstream, where
//! listeners run synchronously on the input thread: blocking the render thread
//! on arbitrary extension code is not acceptable in pir.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use crate::plugins::services::TerminalInputAction;
use crate::plugins::PluginHost;

/// How long the TUI's input loop waits inline for the plugin host before it
/// defers the decision to later loop iterations.
///
/// This wait runs on the render/input thread, so it stays inside a typing
/// frame. Missing it no longer abandons the chunk: [`gate_event_deferred`]
/// returns [`GateOutcome::Deferred`], the loop keeps rendering, and the answer
/// is applied in input order when it arrives (bounded by
/// [`TERMINAL_INPUT_DEFER_BUDGET`]). Kept well above the typical QuickJS actor
/// round trip so ordinary synchronous handlers are decided without a deferral.
pub const TERMINAL_INPUT_SYNC_BUDGET: Duration = Duration::from_millis(5);

/// Upper bound on how long one physical input may stay deferred waiting for a
/// late handler answer.
///
/// This is a TUI-side backstop above the host's own bounds: the JS face is
/// already capped at 5 s by `plugins::host`, and native plugin futures get the
/// short dispatch budget, but a native `UiService::on_terminal_input`
/// subscription is a synchronous callback with no internal bound. Once this
/// elapses the chunk fails open to the built-in path (its late answer is
/// dropped, never applied retroactively) and the router keeps new chunks out of
/// the wedged request's queue until it settles.
///
/// While a chunk is deferred the render loop keeps running; only the *input
/// order* waits. The hold starts when the inline sync wait misses, so one
/// chunk's total wait is [`TERMINAL_INPUT_SYNC_BUDGET`] plus this budget (the
/// loop's poll tick can add one frame on top). Override with
/// [`TERMINAL_INPUT_BUDGET_ENV_VAR`] (`PIR_TUI_TERMINAL_INPUT_BUDGET_MS`).
pub const TERMINAL_INPUT_DEFER_BUDGET: Duration = Duration::from_secs(5);

/// Environment override for [`TERMINAL_INPUT_DEFER_BUDGET`], in milliseconds.
pub const TERMINAL_INPUT_BUDGET_ENV_VAR: &str = "PIR_TUI_TERMINAL_INPUT_BUDGET_MS";

/// Largest accepted [`TERMINAL_INPUT_BUDGET_ENV_VAR`] value (one minute).
const TERMINAL_INPUT_BUDGET_MAX_MS: u64 = 60_000;

/// Resolve the TUI's deferral backstop: the env override when it parses as a
/// positive integer (capped at a minute), else [`TERMINAL_INPUT_DEFER_BUDGET`].
/// Zero, negative and non-numeric values are ignored, matching the other TUI
/// timeout resolvers. The value is parsed wider than the cap, so a huge
/// integer — even one that overflows `u64` — lands on the one-minute cap
/// instead of silently reverting to the default.
pub fn resolve_terminal_input_budget_ms(get: impl FnMut(&str) -> Option<String>) -> u64 {
    let mut get = get;
    let default = TERMINAL_INPUT_DEFER_BUDGET.as_millis() as u64;
    if let Some(raw) = get(TERMINAL_INPUT_BUDGET_ENV_VAR) {
        if let Ok(configured) = raw.trim().parse::<u128>() {
            if configured > 0 {
                return configured.min(u128::from(TERMINAL_INPUT_BUDGET_MAX_MS)) as u64;
            }
        }
    }
    default
}

/// [`resolve_terminal_input_budget_ms`] over the process environment.
pub fn terminal_input_budget_from_env() -> Duration {
    Duration::from_millis(resolve_terminal_input_budget_ms(|key| {
        std::env::var(key).ok()
    }))
}

/// Upper bound on one `terminal_input` dispatch on the handler thread. A
/// handler that exceeds it is abandoned (the JS runtime marks the dispatch
/// ABANDONED through `emit_with_timeout`); the reply for that chunk is `Pass`
/// so the built-in path keeps working.
pub const TERMINAL_INPUT_HANDLER_TIMEOUT: Duration =
    crate::plugins::host::DEFAULT_TERMINAL_INPUT_HANDLER_TIMEOUT;

/// Bounded request queue. When typing outruns a slow handler, chunks beyond
/// this capacity skip the hook and go straight to the built-in path — typing
/// never blocks on the hook queue.
const TERMINAL_INPUT_QUEUE_CAPACITY: usize = 64;

/// Upper bound on one handler `Replace` payload, in bytes.
///
/// A replacement rewrites a *single* physical input chunk, so its legitimate
/// shape is a key sequence or a short string: the largest real terminal chunk
/// (an SGR mouse report, a modified function key) is tens of bytes. 4 KiB is
/// far beyond that while keeping the decode on the input thread trivial. The
/// cap is checked *before* decoding, so a handler echoing a large buffer can
/// neither allocate a matching `Vec<Event>` nor park that work ahead of typing.
///
/// The bound is INCLUSIVE (`replacement_cap_violation` rejects only when
/// `len >` this value), matching the event cap and every other bound in this
/// module.
pub const TERMINAL_INPUT_REPLACEMENT_MAX_BYTES: usize = 4 * 1024;

/// Upper bound on the events decoded from one replacement.
///
/// Plain text decodes to one event per character, and the input loop drains
/// one `pending_input` event per iteration (with a frame per handled event),
/// so this is also the worst-case number of iterations one handler answer can
/// insert in front of real input. 256 is more than a full terminal line and
/// ~10x any key rewrite, while bounding the delay to a handful of frames; an
/// over-cap replacement is dropped instead of partially applied.
///
/// Like the byte cap this bound is INCLUSIVE: exactly this many chars/events
/// is admitted; only `>` is a violation.
pub const TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS: usize = 256;

struct TerminalInputRequest {
    data: String,
    reply: SyncSender<TerminalInputAction>,
}

/// A hook decision that missed the inline sync budget.
///
/// The input loop holds the physical event that produced it and polls
/// [`Self::try_take`] on later iterations; the event must not reach the
/// built-in path while it is pending. Dropping this handle abandons the answer
/// (the dispatch thread's send is ignored), which is the fail-open path after
/// [`Self::deadline`].
pub struct TerminalInputPending {
    reply: Receiver<TerminalInputAction>,
    deadline: Instant,
}

impl TerminalInputPending {
    /// Non-blocking poll. `None` = still pending; `Some(Pass)` also covers a
    /// dispatcher that died before answering (fail open).
    pub fn try_take(&mut self) -> Option<TerminalInputAction> {
        match self.reply.try_recv() {
            Ok(action) => Some(action),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(TerminalInputAction::Pass),
        }
    }

    /// When the deferral backstop expires. The caller fails the held event
    /// open at this instant rather than waiting forever on a wedged handler.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// The outcome of submitting one chunk without blocking on the answer.
pub enum TerminalInputDispatch {
    /// No decision to apply: no handlers, router disabled, the dispatcher is
    /// still wedged on an earlier chunk, the queue is full, or the dispatch
    /// thread is gone. Feed the built-in path.
    Pass,
    /// The host answered inside the inline sync budget.
    Immediate(TerminalInputAction),
    /// The host has not answered yet. Hold the physical event and poll the
    /// pending answer; never fall through for that chunk while it is pending.
    Pending(TerminalInputPending),
}

struct TerminalInputRouterInner {
    tx: SyncSender<TerminalInputRequest>,
    host: Arc<PluginHost>,
    sync_budget: Duration,
    defer_budget: Duration,
    /// Requests handed to the dispatch thread and not answered yet. A wedged
    /// handler makes this non-zero; new chunks then fail open instead of
    /// queueing behind it (the TUI only submits one at a time while it holds a
    /// [`TerminalInputPending`]).
    in_flight: Arc<AtomicUsize>,
    /// Requests actually handed to the dispatch thread (test observability;
    /// the zero-handler fast path leaves this at 0).
    submitted: AtomicUsize,
}

/// Off-thread dispatcher for raw terminal input.
///
/// Constructed with [`TerminalInputRouter::with_host`] once the TUI receives
/// the session's [`PluginHost`]; [`TerminalInputRouter::disabled`] (the
/// pre-session default) makes [`Self::dispatch`] a constant `Pass` with no
/// thread and no allocation.
pub struct TerminalInputRouter {
    inner: Option<Arc<TerminalInputRouterInner>>,
}

impl TerminalInputRouter {
    /// No plugin host yet: every chunk passes through.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Start the dispatch thread for `host` with the production budgets.
    ///
    /// The deferral backstop honors [`TERMINAL_INPUT_BUDGET_ENV_VAR`]; the
    /// inline wait is [`TERMINAL_INPUT_SYNC_BUDGET`], clamped to that total so
    /// a tiny configured budget can never produce a longer inline wait.
    pub fn with_host(host: Arc<PluginHost>) -> Self {
        let defer_budget = terminal_input_budget_from_env();
        Self::with_all_budgets(
            host,
            TERMINAL_INPUT_SYNC_BUDGET.min(defer_budget),
            TERMINAL_INPUT_HANDLER_TIMEOUT,
            defer_budget,
        )
    }

    /// Test seam: explicit budgets so the blocking path can be exercised
    /// without a multi-second test. The deferral backstop is the production
    /// default (not the environment), keeping the test deterministic.
    pub fn with_budgets(
        host: Arc<PluginHost>,
        sync_budget: Duration,
        handler_timeout: Duration,
    ) -> Self {
        Self::with_all_budgets(
            host,
            sync_budget,
            handler_timeout,
            TERMINAL_INPUT_DEFER_BUDGET,
        )
    }

    /// Test seam: all three budgets explicit.
    pub fn with_all_budgets(
        host: Arc<PluginHost>,
        sync_budget: Duration,
        handler_timeout: Duration,
        defer_budget: Duration,
    ) -> Self {
        let (tx, rx) = sync_channel::<TerminalInputRequest>(TERMINAL_INPUT_QUEUE_CAPACITY);
        let thread_host = Arc::clone(&host);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let thread_in_flight = Arc::clone(&in_flight);
        let spawned = std::thread::Builder::new()
            .name("pir-terminal-input".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                else {
                    // No runtime: fail open for every request.
                    while let Ok(request) = rx.recv() {
                        thread_in_flight.fetch_sub(1, Ordering::AcqRel);
                        let _ = request.reply.send(TerminalInputAction::Pass);
                    }
                    return;
                };
                while let Ok(request) = rx.recv() {
                    // Handlers are arbitrary in-process code (a native
                    // `TerminalInputHandler` runs synchronously, right here),
                    // so a panic must fail THIS chunk open rather than kill
                    // the dispatcher: a dead dispatcher permanently disables
                    // the raw-input hooks, and the `in_flight` reservation
                    // below would never be released.
                    let dispatched = super::terminal::catch_contained(|| {
                        if thread_host.has_terminal_input_handlers() {
                            runtime.block_on(thread_host.dispatch_terminal_input_with_timeout(
                                &request.data,
                                handler_timeout,
                            ))
                        } else {
                            TerminalInputAction::Pass
                        }
                    });
                    // Release the busy gate BEFORE the reply: the input loop may
                    // dispatch the next chunk as soon as it observes the answer.
                    thread_in_flight.fetch_sub(1, Ordering::AcqRel);
                    let action = match dispatched {
                        Ok(action) => action,
                        Err(_) => {
                            crate::ext_diag!(
                                "[ext] terminal_input dispatch panicked; falling through"
                            );
                            TerminalInputAction::Pass
                        }
                    };
                    // The TUI may have given up on this chunk (deferral
                    // backstop); its receiver is then gone and the send is
                    // ignored.
                    let _ = request.reply.send(action);
                }
            });
        if spawned.is_err() {
            return Self { inner: None };
        }
        Self {
            inner: Some(Arc::new(TerminalInputRouterInner {
                tx,
                host,
                sync_budget,
                defer_budget,
                in_flight,
                submitted: AtomicUsize::new(0),
            })),
        }
    }

    /// Whether any face has a live handler. Lock-free and allocation-free;
    /// the zero-handler path keeps the pre-hook input cost.
    pub fn has_handlers(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.host.has_terminal_input_handlers())
    }

    /// How many chunks were actually handed to the dispatch thread. The
    /// zero-handler fast path never increments this (test observability).
    #[doc(hidden)]
    pub fn submitted_requests(&self) -> usize {
        self.inner
            .as_ref()
            .map(|inner| inner.submitted.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// The deferral backstop this router applies to a late answer
    /// ([`TerminalInputPending::deadline`]). The disabled router reports the
    /// production default; it never returns a pending answer.
    pub fn defer_budget(&self) -> Duration {
        self.inner
            .as_ref()
            .map(|inner| inner.defer_budget)
            .unwrap_or(TERMINAL_INPUT_DEFER_BUDGET)
    }

    /// Whether a previously submitted request has not been answered yet.
    ///
    /// A deferred chunk keeps this true; once the deferral backstop expires,
    /// new chunks fail open instead of queueing behind a wedged handler.
    pub fn is_busy(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.in_flight.load(Ordering::Acquire) > 0)
    }

    /// Dispatch one raw chunk and return a deferrable decision.
    ///
    /// The caller must wait at most the inline sync budget; a
    /// [`TerminalInputDispatch::Pending`] answer means the physical event that
    /// produced it must be held (not fed to the built-in path) until
    /// [`TerminalInputPending::try_take`] returns or its deadline passes.
    ///
    /// `Pass` covers "no handlers", "queue full", "the dispatcher is wedged on
    /// an earlier chunk" and "dispatch thread gone": every one of those must
    /// feed the built-in path unchanged.
    pub fn dispatch_pending(&self, data: &str) -> TerminalInputDispatch {
        let Some(inner) = self.inner.as_deref() else {
            return TerminalInputDispatch::Pass;
        };
        if !inner.host.has_terminal_input_handlers() {
            return TerminalInputDispatch::Pass;
        }
        // An earlier request is still on the dispatch thread (it outlived its
        // defer budget). Never park this chunk behind it: fail open until the
        // wedged request settles. `try_update` also makes the one-in-flight
        // reservation atomic for any caller.
        if inner
            .in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |in_flight| {
                (in_flight == 0).then_some(1)
            })
            .is_err()
        {
            return TerminalInputDispatch::Pass;
        }
        let (reply_tx, reply_rx) = sync_channel(1);
        if inner
            .tx
            .try_send(TerminalInputRequest {
                data: data.to_owned(),
                reply: reply_tx,
            })
            .is_err()
        {
            // Queue full (a slow handler) or the thread is gone: never block
            // typing on the hook.
            inner.in_flight.fetch_sub(1, Ordering::AcqRel);
            return TerminalInputDispatch::Pass;
        }
        inner.submitted.fetch_add(1, Ordering::Relaxed);
        match reply_rx.recv_timeout(inner.sync_budget) {
            Ok(action) => TerminalInputDispatch::Immediate(action),
            // Late answer: hold the receiver for the loop's deferred path.
            Err(RecvTimeoutError::Timeout) => {
                TerminalInputDispatch::Pending(TerminalInputPending {
                    reply: reply_rx,
                    deadline: Instant::now() + inner.defer_budget,
                })
            }
            // The dispatcher died between accepting the request and replying.
            Err(RecvTimeoutError::Disconnected) => TerminalInputDispatch::Pass,
        }
    }

    /// Dispatch one raw chunk, waiting at most the router's sync budget.
    ///
    /// Legacy synchronous contract: a late answer fails open. The TUI uses
    /// [`Self::dispatch_pending`] so a slow handler keeps its decision.
    pub fn dispatch(&self, data: &str) -> TerminalInputAction {
        match self.dispatch_pending(data) {
            TerminalInputDispatch::Immediate(action) => action,
            TerminalInputDispatch::Pass | TerminalInputDispatch::Pending(_) => {
                TerminalInputAction::Pass
            }
        }
    }
}

/// Result of offering one physical event to [`gate_event_deferred`].
pub enum GateOutcome {
    /// No decision: process the event with the built-in path.
    Pass,
    /// The handler consumed or replaced the event; it must not reach the
    /// built-in path.
    Handled,
    /// The handler missed the inline sync budget. Hold the event and poll the
    /// pending answer; the event must not reach the built-in path while this
    /// is pending.
    Deferred(TerminalInputPending),
}

/// The production gate in front of the built-in input handling.
///
/// Returns [`GateOutcome::Handled`] when the event must not be processed by the
/// built-in path: the handler consumed it, or the handler replaced it (the
/// replacement's decoded events are pushed to the front of `pending` and marked
/// via `bypass` so they are not dispatched to the hooks a second time).
/// Returns [`GateOutcome::Pass`] for pass-through and for events that have no
/// raw-terminal shape. Returns [`GateOutcome::Deferred`] when the host has not
/// answered inside the router's inline sync budget; the caller holds the event
/// and later calls [`apply_deferred_action`] with the answer.
pub fn gate_event_deferred(
    router: &TerminalInputRouter,
    bypass: &mut usize,
    event: &Event,
    pending: &mut VecDeque<Event>,
) -> GateOutcome {
    if *bypass > 0 {
        // A replacement event: the hook already saw the original chunk. No
        // encoding, no dispatch.
        *bypass -= 1;
        return GateOutcome::Pass;
    }
    // Zero-handler fast path: one atomic check and no allocation. This is the
    // ordinary session, so it must stay indistinguishable from the pre-hook
    // input loop.
    if !router.has_handlers() {
        return GateOutcome::Pass;
    }
    let Some(data) = event_input_data(event) else {
        return GateOutcome::Pass;
    };
    match router.dispatch_pending(&data) {
        TerminalInputDispatch::Pass => GateOutcome::Pass,
        TerminalInputDispatch::Immediate(action) => {
            if apply_action(action, bypass, pending) {
                GateOutcome::Handled
            } else {
                GateOutcome::Pass
            }
        }
        TerminalInputDispatch::Pending(pending_reply) => GateOutcome::Deferred(pending_reply),
    }
}

/// Apply the answer to a deferred event.
///
/// Returns `true` when the held event is consumed (consume, replacement, or an
/// over-cap replacement); `false` when it must now run through the built-in
/// path. Replacement events are queued and bypass-marked exactly as in
/// [`gate_event_deferred`].
pub fn apply_deferred_action(
    action: TerminalInputAction,
    bypass: &mut usize,
    pending: &mut VecDeque<Event>,
) -> bool {
    apply_action(action, bypass, pending)
}

/// The legacy synchronous gate: a late answer fails open.
///
/// Kept for callers/tests that do not run the event loop's deferred state
/// machine; the TUI's production path is [`gate_event_deferred`].
pub fn gate_event(
    router: &TerminalInputRouter,
    bypass: &mut usize,
    event: &Event,
    pending: &mut VecDeque<Event>,
) -> bool {
    matches!(
        gate_event_deferred(router, bypass, event, pending),
        GateOutcome::Handled
    )
}

fn apply_action(
    action: TerminalInputAction,
    bypass: &mut usize,
    pending: &mut VecDeque<Event>,
) -> bool {
    match action {
        TerminalInputAction::Pass => false,
        TerminalInputAction::Consume => true,
        TerminalInputAction::Replace(replacement) => {
            // Bound the payload BEFORE decoding: one physical input may add
            // at most this many bytes/events to `pending_input`, no matter
            // how much a handler (or an echo of extension output) returns.
            // Over-cap is CONSUMED (the original never falls through to the
            // built-in path — the handler explicitly rewrote it) with a
            // diagnostic, and nothing is queued, so the remaining input order
            // is untouched.
            if let Some(cap) = replacement_cap_violation(&replacement) {
                match cap {
                    ReplacementCap::Bytes(len) => crate::ext_diag!(
                        "[ext] terminal_input replacement dropped: {} bytes exceeds the {} byte cap",
                        len,
                        TERMINAL_INPUT_REPLACEMENT_MAX_BYTES
                    ),
                    ReplacementCap::Events(count) => crate::ext_diag!(
                        "[ext] terminal_input replacement dropped: {} chars exceeds the {} event cap (checked before decode)",
                        count,
                        TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS
                    ),
                }
                return true;
            }
            let events = replacement_events(&replacement);
            if events.is_empty() {
                return true;
            }
            if events.len() > TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS {
                crate::ext_diag!(
                    "[ext] terminal_input replacement dropped: {} events exceeds the {} event cap",
                    events.len(),
                    TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS
                );
                return true;
            }
            *bypass += events.len();
            for event in events.into_iter().rev() {
                pending.push_front(event);
            }
            // The original must not also reach the built-in path: the
            // replacement is what the focused component should receive.
            true
        }
    }
}

/// The raw string upstream's listener would have received for one parsed
/// event, or `None` when the event has no terminal-input spelling.
pub fn event_input_data(event: &Event) -> Option<String> {
    match event {
        Event::Key(key) => key_event_data(key),
        Event::Mouse(mouse) => Some(mouse_event_data(mouse)),
        Event::Paste(text) => Some(paste_event_data(text)),
        // Focus reports are raw input upstream's listener sees too.
        Event::FocusGained => Some("\x1b[I".to_owned()),
        Event::FocusLost => Some("\x1b[O".to_owned()),
        _ => None,
    }
}

/// Canonical terminal encoding of a crossterm key event.
///
/// Plain characters are themselves; control chords are their control byte;
/// Alt is the legacy `ESC` prefix; CSI keys use the standard xterm spellings.
/// Modified CSI keys carry the `1;<modifier>` parameter. Unknown codes return
/// `None` (no dispatch) rather than a fabricated chunk.
pub fn key_event_data(key: &KeyEvent) -> Option<String> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let modifiers = key.modifiers;
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let modified = alt || ctrl || shift;
    let csi_modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);

    let encoded = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // A control chord with no byte spelling (e.g. ctrl+é):
                // upstream terminals do not produce one either.
                let byte = control_byte(c)? as char;
                if alt {
                    format!("\x1b{byte}")
                } else {
                    byte.to_string()
                }
            } else if alt {
                format!("\x1b{c}")
            } else {
                c.to_string()
            }
        }
        KeyCode::Esc => "\x1b".to_owned(),
        KeyCode::Enter => {
            if alt {
                "\x1b\r".to_owned()
            } else if shift && !ctrl {
                // Shift+Enter is the CSI-u chord upstream's native detection
                // synthesizes (`terminal.ts:17`), and what a Kitty/CSI-u
                // terminal reports for it; the raw chunk an upstream listener
                // sees carries the sequence, not a bare CR.
                crate::tui::stdin_buffer::NATIVE_SHIFT_ENTER_SEQUENCE.to_owned()
            } else {
                "\r".to_owned()
            }
        }
        KeyCode::Tab => {
            if shift {
                // BackTab is its own code, but some terminals report shift+tab
                // as Tab+SHIFT; upstream sees ESC[Z either way.
                "\x1b[Z".to_owned()
            } else {
                "\t".to_owned()
            }
        }
        KeyCode::BackTab => "\x1b[Z".to_owned(),
        KeyCode::Backspace => {
            if ctrl {
                // ctrl+backspace is the DEL/BS control byte pair on terminals
                // that report it separately; 0x08 is the conventional one.
                "\x08".to_owned()
            } else if alt {
                "\x1b\x7f".to_owned()
            } else {
                "\x7f".to_owned()
            }
        }
        KeyCode::Left => csi_key('D', modified, csi_modifier),
        KeyCode::Right => csi_key('C', modified, csi_modifier),
        KeyCode::Up => csi_key('A', modified, csi_modifier),
        KeyCode::Down => csi_key('B', modified, csi_modifier),
        KeyCode::Home => home_end_key('H', modified, csi_modifier),
        KeyCode::End => home_end_key('F', modified, csi_modifier),
        KeyCode::Insert => tilde_key(2, modified, csi_modifier),
        KeyCode::Delete => tilde_key(3, modified, csi_modifier),
        KeyCode::PageUp => tilde_key(5, modified, csi_modifier),
        KeyCode::PageDown => tilde_key(6, modified, csi_modifier),
        KeyCode::F(number) => function_key(number, modified, csi_modifier)?,
        KeyCode::Null => "\0".to_owned(),
        _ => return None,
    };
    Some(encoded)
}

/// Standard SGR mouse report (`ESC [ < Cb ; Cx ; Cy M|m`), the shape
/// `EnableMouseCapture` asks the terminal for and upstream's raw listener
/// receives.
pub fn mouse_event_data(mouse: &MouseEvent) -> String {
    let (base, release) = match mouse.kind {
        MouseEventKind::Down(button) => (button_code(button), false),
        MouseEventKind::Up(button) => (button_code(button), true),
        MouseEventKind::Drag(button) => (button_code(button) + 32, false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    let modifiers = mouse.modifiers;
    let mut cb = base;
    if modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    let final_byte = if release { 'm' } else { 'M' };
    format!(
        "\x1b[<{cb};{};{}{final_byte}",
        mouse.column + 1,
        mouse.row + 1
    )
}

/// Upstream re-wraps paste content with bracketed-paste markers before handing
/// it to the listener; the TUI's `Event::Paste` carries the unwrapped text.
pub fn paste_event_data(text: &str) -> String {
    format!("\x1b[200~{text}\x1b[201~")
}

/// Decode a handler's replacement chunk into the events the built-in path
/// consumes. A complete escape sequence is decoded with crossterm's parser
/// port; plain text becomes one key event per character (so it inserts like
/// typing). An empty replacement yields no events (consumed).
pub fn replacement_events(data: &str) -> Vec<Event> {
    if data.is_empty() {
        return Vec::new();
    }
    if let Some(events) = crate::tui::stdin_buffer::decode_input_sequence(data.as_bytes()) {
        if !events.is_empty() {
            return events;
        }
    }
    data.chars()
        .map(|c| {
            let code = match c {
                '\r' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                '\x1b' => KeyCode::Esc,
                '\x7f' => KeyCode::Backspace,
                _ => KeyCode::Char(c),
            };
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
        })
        .collect()
}

/// Which bound a handler replacement violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementCap {
    /// Payload bytes exceed [`TERMINAL_INPUT_REPLACEMENT_MAX_BYTES`].
    Bytes(usize),
    /// The payload cannot decode within
    /// [`TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS`]: it has more source chars
    /// than the event cap, and every decoded event consumes at least one char.
    Events(usize),
}

/// Check BOTH replacement caps BEFORE the payload is decoded.
///
/// BOUNDARY: both caps are INCLUSIVE maxima. Exactly
/// [`TERMINAL_INPUT_REPLACEMENT_MAX_BYTES`] bytes, and exactly
/// [`TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS`] chars, are admitted; a payload is
/// rejected only when it EXCEEDS a cap (`>`). This is deliberate and uniform:
/// the post-decode event check and `bound_pending_input` use the same `>`,
/// and the diagnostics say "exceeds".
///
/// The byte cap is the direct bound; the event cap is checked against the
/// payload's char count with an early stop. That is sound because every
/// decoded event consumes at least one source char (plain text is one event
/// per char; a complete escape sequence is a single event built from several
/// chars), so a payload with more chars than the event cap can never decode
/// within it. The check costs one bounded scan and no `Vec<Event>` allocation,
/// which is what keeps a handler echoing a large buffer from parking the
/// decode ahead of real typing.
///
/// Because UTF-8 encodes at most 4 bytes per char, a payload at the byte cap
/// necessarily has at least 1024 chars — over the event cap — so the byte
/// check can never be the *sole* rejection at its own boundary. It still runs
/// first (precedence: an oversized echo is classified `Bytes` without a char
/// scan) and it keeps the direct bound correct if the event cap ever moves.
pub fn replacement_cap_violation(data: &str) -> Option<ReplacementCap> {
    if data.len() > TERMINAL_INPUT_REPLACEMENT_MAX_BYTES {
        return Some(ReplacementCap::Bytes(data.len()));
    }
    let chars = data
        .chars()
        .take(TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS + 1)
        .count();
    if chars > TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS {
        return Some(ReplacementCap::Events(chars));
    }
    None
}

fn button_code(button: crossterm::event::MouseButton) -> u16 {
    match button {
        crossterm::event::MouseButton::Left => 0,
        crossterm::event::MouseButton::Middle => 1,
        crossterm::event::MouseButton::Right => 2,
    }
}

fn csi_key(final_byte: char, modified: bool, modifier: u8) -> String {
    if modified {
        format!("\x1b[1;{modifier}{final_byte}")
    } else {
        format!("\x1b[{final_byte}")
    }
}

fn home_end_key(final_byte: char, modified: bool, modifier: u8) -> String {
    if modified {
        format!("\x1b[1;{modifier}{final_byte}")
    } else {
        format!("\x1b[{final_byte}")
    }
}

fn tilde_key(number: u8, modified: bool, modifier: u8) -> String {
    if modified {
        format!("\x1b[{number};{modifier}~")
    } else {
        format!("\x1b[{number}~")
    }
}

fn function_key(number: u8, modified: bool, modifier: u8) -> Option<String> {
    let (final_byte, tilde_number) = match number {
        1 => ('P', None),
        2 => ('Q', None),
        3 => ('R', None),
        4 => ('S', None),
        5 => ('~', Some(15)),
        6 => ('~', Some(17)),
        7 => ('~', Some(18)),
        8 => ('~', Some(19)),
        9 => ('~', Some(20)),
        10 => ('~', Some(21)),
        11 => ('~', Some(23)),
        12 => ('~', Some(24)),
        _ => return None,
    };
    Some(match tilde_number {
        Some(number) => tilde_key(number, modified, modifier),
        None if modified => format!("\x1b[1;{modifier}{final_byte}"),
        None => format!("\x1bO{final_byte}"),
    })
}

/// `ctrl+<char>` as the byte a terminal sends (upstream's control-byte table).
fn control_byte(c: char) -> Option<u8> {
    if c.is_ascii_alphabetic() {
        Some(c.to_ascii_uppercase() as u8 & 0x1f)
    } else {
        match c {
            ' ' => Some(0x00),
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' => Some(0x1e),
            '_' => Some(0x1f),
            _ => None,
        }
    }
}
