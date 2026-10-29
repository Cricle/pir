//! Timers the session and harness arm through the host's `setTimer`.
//!
//! Ports upstream `src/timers.ts`. The TypeScript version distinguishes a
//! *foreground* timer (paces work; must fire) from a *background* one (only
//! frees memory; may be declined). Node's `unref()` is what makes a background
//! timer not keep the process alive; Rust has no equivalent, so the native
//! implementation here cannot honor `unref()`. A host that cannot either may
//! decline a background timer by returning `None` (upstream returns
//! `undefined`).
//!
//! Delays beyond [`MAX_TIMER_DELAY_MS`] are waited out in steps, matching the
//! upstream `setTimeout` clamp.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Longest delay `setTimeout` supports; a longer one is waited for in steps.
pub const MAX_TIMER_DELAY_MS: i64 = 2_147_483_647;

/// A callback a timer runs.
pub type TimerCallback = Arc<dyn Fn() + Send + Sync>;

/// A disarmed timer. Calling it more than once is safe and a no-op after the
/// first call.
///
/// Upstream returns a plain `() => void`; this struct is the same contract with
/// a name, so a caller can store it in an `Option` and declare which it got.
pub struct TimerCancel(Box<dyn FnOnce() + Send + Sync>);

impl TimerCancel {
    /// Wrap a disarm function.
    pub fn new(cancel: Box<dyn FnOnce() + Send + Sync>) -> Self {
        Self(cancel)
    }

    /// Disarm the timer. Consumes the cancel token, so a second call is a
    /// compile error rather than a silently repeated host call.
    pub fn cancel(self) {
        (self.0)();
    }
}

impl std::fmt::Debug for TimerCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TimerCancel")
    }
}

/// A timer that can be disarmed, and can report whether it already was.
///
/// The cancellation flag is what a native timer polls; the optional disarm
/// token is the host's own cancel and runs exactly once. Both are reachable
/// through a single [`TimerHandle`] so a caller never has to know which
/// implementation armed the timer.
pub struct TimerHandle {
    cancelled: Arc<AtomicBool>,
    disarm: std::sync::Mutex<Option<TimerCancel>>,
}

impl TimerHandle {
    /// Stop the timer. Idempotent across threads; the host disarm token runs
    /// at most once.
    pub fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let token = self
            .disarm
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(token) = token {
            token.cancel();
        }
    }

    /// Whether the timer has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// A cancel token that cancels this handle. Consuming it makes the
    /// upstream `() => void` shape available to hosts that only hold a token.
    pub fn into_cancel(self) -> TimerCancel {
        let handle = Arc::new(self);
        TimerCancel::new(Box::new(move || handle.cancel()))
    }

    /// Build a handle around a host cancel token.
    fn from_cancel(cancel: TimerCancel) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            disarm: std::sync::Mutex::new(Some(cancel)),
        }
    }
}

impl std::fmt::Debug for TimerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// One timer request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerOptions {
    /// Whether the timer only frees memory and may be declined.
    pub background: bool,
}

impl TimerOptions {
    /// A timer that paces or resumes work.
    pub const FOREGROUND: Self = Self { background: false };
    /// A timer that only frees memory.
    pub const BACKGROUND: Self = Self { background: true };
}

/// A host-provided timer factory, the Rust form of upstream `SetTimer`.
///
/// Returns `None` when the host declines the timer (the Rust analogue of the
/// upstream `undefined` return). A panic in `set` is contained and treated
/// exactly like a decline, so a host that throws in TypeScript and one that
/// panics in Rust have the same effect. A host that wants background-only
/// semantics declines `background` requests.
pub trait SetTimer: Send + Sync {
    /// Arm `callback` after `delay_ms`; `None` declines it.
    ///
    /// Panicking is treated as declining: [`Timers`] catches the unwind, so a
    /// host panic never escapes it. The returned token disarms the timer. A
    /// host must tolerate a disarm after the callback already ran.
    fn set(
        &self,
        callback: TimerCallback,
        delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel>;
}

/// A `SetTimer` implemented by a closure, for hosts and test fakes.
pub struct SetTimerFn<F>(pub F);

impl<F> SetTimer for SetTimerFn<F>
where
    F: Fn(TimerCallback, i64, TimerOptions) -> Option<TimerCancel> + Send + Sync,
{
    fn set(
        &self,
        callback: TimerCallback,
        delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel> {
        (self.0)(callback, delay_ms, options)
    }
}

/// Timers the session and harness arm through the host's `setTimer`.
#[derive(Clone)]
pub struct Timers {
    host: Option<Arc<dyn SetTimer>>,
    report: Arc<dyn Fn(String) + Send + Sync>,
    reported: Arc<AtomicBool>,
}

impl std::fmt::Debug for Timers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timers")
            .field("has_host", &self.host.is_some())
            .finish()
    }
}

impl Timers {
    /// Wrap an optional host timer factory.
    ///
    /// A foreground timer the host declines is armed with the native timer
    /// instead, and the first such failure is reported once. A background
    /// timer the host declines stays declined.
    pub fn new(
        set_timer: Option<Arc<dyn SetTimer>>,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        Self {
            host: set_timer,
            report,
            reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Timers backed only by the native implementation.
    ///
    /// Construction does not touch a Tokio runtime; only arming a timer does.
    /// This is upstream's `nativeTimer` as the default `HarnessOptions
    /// .setTimer`.
    pub fn native(report: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        Self::new(None, report)
    }

    /// Wrap a host `setTimer`, falling back to the native timer.
    ///
    /// Upstream `hostTimers(setTimer, report)`. Construction does not touch a
    /// Tokio runtime; only arming a timer does, so this can be built while
    /// describing options, before the runtime the session will run on exists.
    pub fn host_timers(
        set_timer: Option<Arc<dyn SetTimer>>,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        Self::new(set_timer, report)
    }

    /// A timer that paces or resumes work. When the host declines it, the
    /// native timer arms it instead.
    pub fn foreground(&self, callback: TimerCallback, delay_ms: i64) -> TimerHandle {
        if let Some(host) = &self.host {
            if let Some(cancel) = host_set(
                host.as_ref(),
                Arc::clone(&callback),
                delay_ms,
                TimerOptions::FOREGROUND,
            ) {
                return TimerHandle::from_cancel(cancel);
            }
            self.report_once(
                "HarnessOptions.setTimer declined a timer that paces work; using the native timer",
            );
        }
        native_timer(callback, delay_ms)
    }

    /// A timer that only frees memory. `None` when the host declines it.
    pub fn background(&self, callback: TimerCallback, delay_ms: i64) -> Option<TimerHandle> {
        match &self.host {
            Some(host) => host_set(host.as_ref(), callback, delay_ms, TimerOptions::BACKGROUND)
                .map(TimerHandle::from_cancel),
            // The native implementation accepts background timers: Rust has no
            // `unref()`, and refusing to arm would silently stop the work the
            // caller asked to defer. A deployment where a pending timer blocks
            // eviction (Cloudflare Workers upstream) supplies a host that
            // declines them instead.
            None => Some(native_timer(callback, delay_ms)),
        }
    }

    fn report_once(&self, message: &str) {
        if !self.reported.swap(true, Ordering::SeqCst) {
            (self.report)(message.to_owned());
        }
    }
}

/// Call the host's `set`, treating a panic as a declined timer.
///
/// The callback is `Send + Sync` but not necessarily `UnwindSafe`, so the host
/// call is wrapped in [`AssertUnwindSafe`]; a panic crossing the boundary is
/// still contained here, which is the behavior the caller depends on. The
/// caller interprets `None` exactly as it would a host decline.
fn host_set(
    host: &dyn SetTimer,
    callback: TimerCallback,
    delay_ms: i64,
    options: TimerOptions,
) -> Option<TimerCancel> {
    catch_unwind(AssertUnwindSafe(|| host.set(callback, delay_ms, options))).unwrap_or(None)
}

/// Arm a native timer with the same stepping behavior as upstream's
/// `nativeTimer`.
///
/// The callback runs on a Tokio runtime task, so this must be called from
/// inside a runtime (the durable module's async callers always are).
pub fn native_timer(callback: TimerCallback, delay_ms: i64) -> TimerHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    tokio::spawn(async move {
        for step in delay_steps(delay_ms) {
            tokio::time::sleep(Duration::from_millis(step as u64)).await;
            if flag.load(Ordering::SeqCst) {
                return;
            }
        }
        if flag.load(Ordering::SeqCst) {
            return;
        }
        callback();
    });
    TimerHandle {
        cancelled,
        disarm: std::sync::Mutex::new(None),
    }
}

/// The delay steps a native timer waits through, in order.
///
/// Exposed as a pure function so the clamping rule is testable without a
/// runtime: a delay of `n` milliseconds becomes `ceil(n / MAX)` steps, each at
/// most [`MAX_TIMER_DELAY_MS`]. A non-positive delay is one zero-length step.
///
/// The steps are produced lazily. Materializing them would allocate one `i64`
/// per `MAX_TIMER_DELAY_MS`, so a caller asking to sleep for `i64::MAX`
/// milliseconds — 292 million years, but a perfectly legal `i64` — would
/// allocate roughly 34 GiB and abort the process. Upstream gets laziness for
/// free from the `while` loop in its `setTimeout` wrapper.
pub fn delay_steps(delay_ms: i64) -> impl Iterator<Item = i64> {
    let remaining = delay_ms.max(0);
    std::iter::successors(Some(remaining), move |left| {
        let taken = (*left).min(MAX_TIMER_DELAY_MS);
        let rest = *left - taken;
        (rest > 0).then_some(rest)
    })
    .map(|left| left.min(MAX_TIMER_DELAY_MS))
}
