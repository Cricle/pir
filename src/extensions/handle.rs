//! Thread-safe handle in front of the pinned QuickJS runtime (the "runtime
//! actor").
//!
//! rquickjs `AsyncRuntime`/`AsyncContext` are `!Send`/`!Sync`: QuickJS state
//! is pinned to the thread that created it. Hosts nevertheless need to drive
//! extensions from arbitrary async tasks (rpc prompt spawns, TUI worker
//! threads, server connection tasks). The actor pattern resolves this:
//!
//! - A dedicated OS thread owns the [`ExtensionRuntime`] and a
//!   `current_thread` tokio runtime. The runtime is *constructed* on that
//!   thread and never moved across a thread boundary.
//! - The public [`ExtensionHandle`] is a `Send + Sync` command mailbox
//!   (tokio `mpsc` request channel + per-request `oneshot` reply channel).
//! - The actor serializes all QuickJS work. rquickjs's `WithFuture` releases
//!   the runtime lock while its inner future is pending, so a long tool call
//!   yields to the executor; concurrent commands still run one after another,
//!   which mirrors upstream's single-threaded JS semantics.
//!
//! Fast synchronous reads (tool defs, flags, notification drain, abort flag)
//! never enter the mailbox: they share the runtime's `RuntimeState` mutex
//! directly, so they work even while a JS call is in flight.
//!
//! ## Plan-only handles (builtin-only extension plans)
//!
//! A host whose extension path plan contains only `builtin:<name>` entries
//! (and no on-disk JS extension path) needs the handle's shared state, path
//! plan and fast-path accessors — but not the QuickJS engine. For that case
//! [`ExtensionHandle::start_plan_only`] builds the handle WITHOUT an actor
//! thread and WITHOUT constructing the QuickJS `ExtensionRuntime` (engine +
//! 223 KB JS prelude, ~10 MB resident). Its JS commands answer exactly like a
//! started actor with nothing loaded: `emit`/`emit_read_only` report no
//! handlers, registry snapshots are empty, renderer/completion queries come
//! back empty, and an on-disk `load` (impossible by construction) is a loud
//! error. Rust builtin registration and dispatch ride the shared `RuntimeState`
//! and the unified plugin host unchanged.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};

#[cfg(feature = "extensions")]
use crate::extensions::runtime::ExtensionRuntime;
use crate::extensions::runtime::{
    CommandOutcome, EmitOutcome, ExtensionConfig, LoadResult, Notification, RendererCallOutcome,
    RendererOutcome, RuntimeState, SessionContextSnapshot, ShortcutDef, ToolDef, ToolResult,
};
use crate::extensions::types::LoadedExtension;

// ---------------------------------------------------------------------------
// Live session streaming flag (ctx.waitForIdle support)
// ---------------------------------------------------------------------------
//
// `ctx.waitForIdle()` (upstream ExtensionCommandContext.waitForIdle,
// extensions/types.ts:405-406) resolves when no prompt run is in flight. The
// QuickJS host fns are 'static, so the live flag cannot ride the per-runtime
// `RuntimeState` borrow; it lives in a shared cell keyed by the runtime's
// state `Arc` pointer identity that the agent session pushes on every run
// start/end ([`ExtensionHandle::set_session_streaming`]) and the `waitForIdle`
// host fn (extensions/runtime/host_functions/context_fns.rs) polls/awaits.
//
// Keying by the runtime's state Arc pointer makes the cell PER-RUNTIME: two
// concurrently live runtimes in one process (server sessions, subagent
// children, TUI + headless) no longer observe each other's streaming state.
// Sequential runtimes that share one state Arc (tests, `/reload` reusing the
// same state) share their cell, with the idle default and a stale `false`
// left behind by a torn-down session being benign. The key is stable for the
// state Arc's lifetime (an actor can never collide with a live one) and the
// entry is removed when the actor thread exits ([`ActorLiveness`]), so a
// recycled heap address always starts from a fresh idle cell.
//
// Plan-only handles (builtin-only plans, no actor) never register a cell:
// `set_session_streaming` skips the push because no JS `waitForIdle`/`isIdle`
// caller can exist, and `ExtensionHandle::drop` removes any cell keyed by
// their state Arc defensively. That keeps the same recycled-address invariant
// for handles without an `ActorLiveness` guard.
pub(crate) struct SessionStreamShared {
    pub streaming: bool,
    pub idle_notify: Arc<tokio::sync::Notify>,
}

/// The keyed shared cells, built once per process (`Arc::new` is not const,
/// so a plain `static` initializer cannot construct the `Notify` handles).
fn lock_session_stream_cells(
) -> std::sync::MutexGuard<'static, std::collections::HashMap<usize, SessionStreamShared>> {
    static CELLS: std::sync::OnceLock<
        StdMutex<std::collections::HashMap<usize, SessionStreamShared>>,
    > = std::sync::OnceLock::new();
    CELLS
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The cell key for one extension runtime: the address of its shared
/// `RuntimeState` `Arc` (stable for the Arc's lifetime; both the pushing
/// handle and the host-fn closures hold clones of the same Arc).
pub(crate) fn session_stream_key(state: &Arc<StdMutex<RuntimeState>>) -> usize {
    Arc::as_ptr(state) as *const u8 as usize
}

/// Push the streaming flag into the runtime's cell and wake idle waiters on
/// the streaming→idle transition.
pub(crate) fn session_stream_push(key: usize, streaming: bool) {
    let notified = {
        let mut cells = lock_session_stream_cells();
        let shared = cells.entry(key).or_insert_with(|| SessionStreamShared {
            streaming: false,
            idle_notify: Arc::new(tokio::sync::Notify::new()),
        });
        shared.streaming = streaming;
        Arc::clone(&shared.idle_notify)
    };
    if !streaming {
        notified.notify_waiters();
    }
}

/// Snapshot the streaming flag plus the wake handle for `waitForIdle` waiters.
/// A missing cell (runtime never pushed, or already torn down) is idle.
#[cfg(feature = "extensions")]
pub(crate) fn session_stream_snapshot(key: usize) -> (bool, Arc<tokio::sync::Notify>) {
    let mut cells = lock_session_stream_cells();
    let shared = cells.entry(key).or_insert_with(|| SessionStreamShared {
        streaming: false,
        idle_notify: Arc::new(tokio::sync::Notify::new()),
    });
    (shared.streaming, Arc::clone(&shared.idle_notify))
}

/// Drop the runtime's cell (actor-thread exit guard): a later runtime whose
/// state `Arc` reuses the address must not inherit a stale streaming flag.
pub(crate) fn session_stream_remove(key: usize) {
    lock_session_stream_cells().remove(&key);
}

/// How many per-runtime session-streaming cells are registered process-wide
/// (test seam for the plan-only leak).
pub(crate) fn session_stream_cell_count() -> usize {
    lock_session_stream_cells().len()
}

/// The registered cell's streaming flag for one runtime, or `None` when the
/// runtime has no cell (the fresh idle default). Reads WITHOUT inserting,
/// unlike [`session_stream_snapshot`], so a probe cannot create the entry it
/// is checking for.
pub(crate) fn session_stream_cell_state(state: &Arc<StdMutex<RuntimeState>>) -> Option<bool> {
    lock_session_stream_cells()
        .get(&session_stream_key(state))
        .map(|shared| shared.streaming)
}

mod accessors;
#[cfg(feature = "extensions")]
mod actor;
mod commands;
mod mailbox;

#[cfg(feature = "extensions")]
use actor::run_actor;
pub(crate) use commands::Command;

/// A `Send + Sync` front of the pinned extension runtime.
///
/// Clones share one `RuntimeState` (and, for a runtime-backed handle, one
/// actor mailbox). Dropping the last clone closes the mailbox and the actor
/// shuts down, dropping the QuickJS runtime on its own thread. A plan-only
/// handle (builtin-only extension plan) has no mailbox or actor at all; see
/// [`ExtensionHandle::start_plan_only`].
#[derive(Clone)]
pub struct ExtensionHandle {
    /// tokio's `Sender` is already cheaply clonable; no `Arc` wrapper needed.
    /// `None` for a plan-only handle: no actor thread (and therefore no
    /// QuickJS runtime) was started because the extension plan carries only
    /// `builtin:<name>` entries.
    tx: Option<mpsc::Sender<Command>>,
    state: Arc<StdMutex<RuntimeState>>,
    /// Successfully loaded extensions, updated by the actor after `load`.
    loaded: Arc<StdMutex<Vec<LoadedExtension>>>,
    /// Whether the actor reported readiness (construction succeeded).
    ready: Arc<AtomicBool>,
    /// The resolved extension path plan this runtime was loaded with (CLI
    /// paths first, then settings/default paths, `builtin:<name>` entries
    /// included in order). The unified plugin host derives the plugin
    /// dispatch order from it.
    path_plan: Arc<RwLock<Vec<PathBuf>>>,
    /// Queued-or-executing host commands (see
    /// [`RuntimeState::pending_commands`]); the caller side of the
    /// background-preemption rule.
    pending_commands: Arc<std::sync::atomic::AtomicUsize>,
    /// Instant (monotonic ms) after which a background JS job must yield to a
    /// queued host command (see [`RuntimeState::preempt_background_at_ms`]).
    preempt_background_at_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Per-dispatch epoch source shared with the actor.
    next_dispatch_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Epochs whose caller stopped waiting; their late side effects are
    /// dropped by the actor.
    abandoned_dispatch_epochs: Arc<StdMutex<std::collections::BTreeSet<u64>>>,
    /// Live `ctx.ui.onTerminalInput` handler count (the O(1) fast-path gate
    /// the TUI reads per physical input; see
    /// [`Self::has_terminal_input_handlers`]).
    terminal_input_handlers: Arc<std::sync::atomic::AtomicUsize>,
}

impl std::fmt::Debug for ExtensionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionHandle").finish_non_exhaustive()
    }
}

impl ExtensionHandle {
    /// Whether this handle owns a live QuickJS extension runtime.
    ///
    /// `false` for a plan-only handle: builtin registration/dispatch and the
    /// fast-path state accessors work exactly as before, but no actor thread
    /// and no `ExtensionRuntime` (QuickJS + prelude) exist. Regression seam for
    /// the builtin-only startup cost (see
    /// [`Self::start_plan_only`]).
    pub fn has_js_runtime(&self) -> bool {
        self.tx.is_some()
    }

    /// Test seam: how many per-runtime session-streaming cells are currently
    /// registered process-wide.
    ///
    /// A plan-only handle must never add one (it has no actor, hence no
    /// `ActorLiveness` guard to remove the entry); the focused plan-only test
    /// asserts the count returns to its baseline after a streaming
    /// transition + drop.
    #[doc(hidden)]
    pub fn session_stream_cell_count() -> usize {
        crate::extensions::handle::session_stream_cell_count()
    }

    /// Test seam: this handle's registered streaming cell state, or `None`
    /// when it has no cell (the same idle default
    /// [`session_stream_snapshot`] returns for a missing entry).
    ///
    /// Unlike the snapshot used by `ctx.waitForIdle`, this probe does not
    /// insert a cell, so it cannot itself create the entry a test checks for.
    #[doc(hidden)]
    pub fn session_stream_cell_state(&self) -> Option<bool> {
        crate::extensions::handle::session_stream_cell_state(&self.state)
    }

    /// Publish the terminal-relative rectangle the TUI rendered for a
    /// floating native custom overlay (`ctx.ui.custom` with `overlay: true`).
    ///
    /// This is the native twin of the JS bridge's bounds echo
    /// (`JsOverlayComponent` mirrors `entry.bounds` into every render
    /// request). A native [`crate::extensions::NativeOverlayHandle`] shares
    /// its bounds cell here when the UI service admits the request, so the
    /// TUI only needs this one runtime call after
    /// `ExternalOverlay::update_bounds` — no signature change anywhere. The
    /// TUI calls it on every changed rectangle and again when a hidden
    /// overlay is shown; hiding/closing clears the cell through
    /// [`Self::clear_native_overlay_bounds`].
    ///
    /// Returns whether a live native handle was waiting for `id`. Hosts and
    /// tests can also drive it directly.
    pub fn publish_native_overlay_bounds(
        &self,
        id: u64,
        bounds: crate::extensions::NativeOverlayBounds,
    ) -> bool {
        crate::extensions::overlay::publish_bounds(session_stream_key(&self.state), id, bounds)
    }

    /// Clear the terminal-relative rectangle published for a floating native
    /// custom overlay.
    ///
    /// The TUI calls this when the overlay stops painting: `setHidden(true)`,
    /// `hide()`, or the close path (`ExternalOverlay::dispose`). A cleared
    /// [`crate::extensions::NativeOverlayHandle::get_bounds`] reports `None`
    /// instead of the last painted rectangle. The binding stays live, so
    /// showing the overlay again republishes the current geometry through
    /// [`Self::publish_native_overlay_bounds`].
    ///
    /// Returns whether a live native handle was waiting for `id`; `false` also
    /// covers JS-face and editor-slot overlays, which never publish bounds.
    pub fn clear_native_overlay_bounds(&self, id: u64) -> bool {
        crate::extensions::overlay::clear_bounds(session_stream_key(&self.state), id)
    }

    /// Host-side registration of one native overlay's bounds cell (called by
    /// `NativeOverlayHandle::bind` while the UI service builds the request).
    pub(crate) fn register_native_overlay_bounds(
        &self,
        id: u64,
        cell: Arc<StdMutex<Option<crate::extensions::NativeOverlayBounds>>>,
    ) {
        crate::extensions::overlay::register_bounds(session_stream_key(&self.state), id, cell);
    }
}

/// Plan-only handles never register a session-stream cell
/// ([`ExtensionHandle::set_session_streaming`] skips the push), but remove any
/// cell keyed by the state `Arc` on drop as a second line of defense: with no
/// actor thread there is no [`ActorLiveness`] guard, and a future code path
/// that registers a cell must not outlive the handle. Actor-backed handles
/// keep the existing ownership: their cell is removed by the actor thread's
/// exit guard, so this `Drop` does nothing for them.
impl Drop for ExtensionHandle {
    fn drop(&mut self) {
        // NOTE: do NOT prune the overlay bounds table here. `Drop` runs for
        // EVERY clone, not only the last one, and the custom-overlay
        // completion callback (a short-lived clone) may be dropped while the
        // overlay is still live. The table stores `Weak` cells and prunes dead
        // entries on registration/publish instead.
        if self.tx.is_none() {
            crate::extensions::handle::session_stream_remove(
                crate::extensions::handle::session_stream_key(&self.state),
            );
        }
    }
}

/// Re-export for hosts that need the default timeout.
pub use crate::extensions::runtime::DEFAULT_CALL_TIMEOUT as EXTENSION_CALL_TIMEOUT;
