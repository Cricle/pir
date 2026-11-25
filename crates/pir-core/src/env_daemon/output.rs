//! The single writer of stdout. Control frames (results, errors, pings, watch events) always go before bulk command
//! output, so a slow link delays output but never a cancel's result or a ping. Bulk output is bounded: readers of
//! commands without an output window wait while too much of it is unsent, which slows the command down like a full
//! pipe would.
//!
//! # Queue bounds
//!
//! The bulk lane is a bounded, backpressured queue: [`BULK_LIMIT`] bytes of
//! unsent output park the producing command reader ([`Output::wait_for_room`]),
//! so a slow link slows the command down like a full pipe — Node's child
//! stdio contract, and the shape this daemon already had.
//!
//! The control lane is deliberately NOT parked: it is the priority lane that
//! carries a cancel's result and the liveness ping, and a producer parked
//! behind a full control queue could no longer report a ping (the ping thread
//! would stall and the client's silence limit could never fire). It is also
//! self-limiting in practice: every frame in it answers a request the client
//! itself issued (or a watcher notification, which the watcher's settle window
//! already debounces), so its depth tracks the client's own outstanding
//! requests. It is therefore bounded by observation instead of by parking: a
//! depth past [`CONTROL_WARN_DEPTH`] frames or past the bulk limit bytes emits
//! one `ext_diag` per doubling of the running total — never silent, and never
//! able to stall the liveness path.

use crate::env_daemon::frame::{self, Frame};
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Unsent bulk bytes above which unwindowed command output waits.
const BULK_LIMIT: usize = 4 * 1024 * 1024;

/// Control-queue depth (frames) past which the daemon reports that the client
/// is not draining. 4096 frames ≈ thousands of small replies/messages; well
/// past any healthy client's in-flight request count, so crossing it means the
/// link is stalled.
const CONTROL_WARN_DEPTH: usize = 4096;

/// Cadence of the control-lane and bulk-park diagnostics: the first crossing
/// is always reported, then one line per this many further occurrences
/// (identical repeats are additionally collapsed by the logging router).
const REPORT_EVERY: u64 = 1024;

#[derive(Default)]
struct Queues {
    control: VecDeque<Frame>,
    /// Bytes of control frames queued (the lane's memory observation; control
    /// frames are small — results ride the bulk lane past
    /// `CONTROL_PAYLOAD` — but a stalled link can queue many of them).
    control_bytes: usize,
    bulk: VecDeque<(Frame, Option<Arc<AtomicUsize>>)>,
    bulk_bytes: usize,
    closed: bool,
}

#[derive(Default)]
pub struct Output {
    queues: Mutex<Queues>,
    /// Signalled when a frame is queued or the writer stops.
    queued: Condvar,
    /// Signalled when bulk bytes were written.
    drained: Condvar,
    /// Diagnostics cadence counters (see [`REPORT_EVERY`]).
    control_warned: AtomicU64,
    room_waits: AtomicU64,
}

impl Output {
    pub fn control(&self, frame: Frame) {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Payload bytes only: computing the JSON text's size would cost a
        // serialization per frame, and the frame COUNT below already bounds
        // the per-frame JSON overhead.
        queues.control_bytes += frame.payload.len();
        queues.control.push_back(frame);
        let depth = queues.control.len();
        let bytes = queues.control_bytes;
        let stalled = depth >= CONTROL_WARN_DEPTH || bytes >= BULK_LIMIT;
        drop(queues);
        // Observation only: the control lane is never parked (see the module
        // docs), so a stalled link has to be visible in the log instead.
        if stalled {
            let crossings = self.control_warned.fetch_add(1, Ordering::Relaxed) + 1;
            if crossings == 1 || crossings.is_multiple_of(REPORT_EVERY) {
                crate::ext_diag!(
                    "[env-daemon] control queue at {} frames / {} bytes (client not draining; {} crossings)",
                    depth,
                    bytes,
                    crossings
                );
            }
        }
        self.queued.notify_one();
    }

    /// Queue bulk output; `unsent` counts this sender's frames until they are written.
    pub fn bulk(&self, frame: Frame, unsent: Option<Arc<AtomicUsize>>) {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues.bulk_bytes += frame.payload.len();
        if let Some(counter) = &unsent {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        queues.bulk.push_back((frame, unsent));
        self.queued.notify_one();
    }

    /// Wait while more than the bulk limit is unsent, unless `stop` is set.
    pub fn wait_for_room(&self, stop: &AtomicBool) {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut waited = false;
        while queues.bulk_bytes > BULK_LIMIT && !queues.closed && !stop.load(Ordering::SeqCst) {
            if !waited {
                waited = true;
                let waits = self.room_waits.fetch_add(1, Ordering::Relaxed) + 1;
                if waits == 1 || waits.is_multiple_of(REPORT_EVERY) {
                    crate::ext_diag!(
                        "[env-daemon] bulk output at {} bytes unsent; a command reader is parked (backpressure, {} waits)",
                        queues.bulk_bytes,
                        waits
                    );
                }
            }
            queues = self
                .drained
                .wait_timeout(queues, Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    pub fn close(&self) {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues.closed = true;
        self.queued.notify_all();
        self.drained.notify_all();
    }

    /// Write frames to `writer` until closed or a write fails.
    pub fn run(&self, writer: &mut impl Write) {
        loop {
            let (frame, unsent) = {
                let mut queues = self
                    .queues
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if let Some(frame) = queues.control.front() {
                        queues.control_bytes -= frame.payload.len();
                        let frame = queues.control.pop_front().unwrap();
                        break (frame, None);
                    }
                    if let Some((frame, unsent)) = queues.bulk.pop_front() {
                        queues.bulk_bytes -= frame.payload.len();
                        break (frame, unsent);
                    }
                    if queues.closed {
                        return;
                    }
                    queues = self
                        .queued
                        .wait(queues)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            let failed = frame::write_frame(writer, &frame).is_err();
            if let Some(counter) = unsent {
                counter.fetch_sub(1, Ordering::SeqCst);
            }
            self.drained.notify_all();
            if failed {
                self.close();
                return;
            }
        }
    }
}
