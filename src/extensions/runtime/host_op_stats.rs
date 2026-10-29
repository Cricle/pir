//! `PI_TIMING=1` attribution of blocking host operations.
//!
//! The per-handler timing lines say *which extension handler* was slow; these
//! counters say *which host operation* it was waiting on (a `node:fs` scan, a
//! `child_process` wait, an HTTP round trip). Every host-bridge entry
//! (`__pir_host.node`, `nodeExt`, `http`) records its elapsed time here when
//! the timing gate is on. Individual slow calls are logged immediately; a
//! background JS job (a promise continuation with no dispatch in flight) also
//! logs a one-line summary of the operations it performed, so an
//! advertisement/agent-discovery scan is attributable even when every single
//! call is short.
//!
//! No payload, path or argument ever reaches the log: labels are the static
//! `<module>.<op>` pair.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A single host op slower than this is logged as it happens.
const SLOW_HOST_OP: Duration = Duration::from_millis(50);

/// One op class' accumulated cost.
#[derive(Debug, Default, Clone)]
struct OpStats {
    label: String,
    calls: u64,
    total_us: u128,
    max_us: u128,
}

#[derive(Debug, Default)]
struct Stats {
    entries: Vec<OpStats>,
}

fn stats() -> &'static Mutex<Stats> {
    static STATS: OnceLock<Mutex<Stats>> = OnceLock::new();
    STATS.get_or_init(|| Mutex::new(Stats::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Stats> {
    stats()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record one completed host call. A no-op (one cached bool) when
/// `PI_TIMING=1` is off.
pub(crate) fn record(label: &str, elapsed: Duration) {
    if !crate::config::timings::timing_enabled_cached() {
        return;
    }
    let micros = elapsed.as_micros();
    if elapsed >= SLOW_HOST_OP {
        // Static op names only; never the payload/path.
        eprintln!("pir-timing: ext.host.{label}: {}ms", elapsed.as_millis());
    }
    let mut guard = lock();
    if let Some(entry) = guard.entries.iter_mut().find(|entry| entry.label == label) {
        entry.calls += 1;
        entry.total_us += micros;
        entry.max_us = entry.max_us.max(micros);
    } else {
        guard.entries.push(OpStats {
            label: label.to_owned(),
            calls: 1,
            total_us: micros,
            max_us: micros,
        });
    }
}

/// Snapshot and reset the accumulated stats as a one-line summary of the most
/// expensive op classes, or `None` when nothing was recorded. Called at the
/// end of a long background job; resetting keeps a slow scan from being
/// re-reported by every later idle drain.
pub(crate) fn take_summary() -> Option<String> {
    if !crate::config::timings::timing_enabled_cached() {
        return None;
    }
    let mut guard = lock();
    if guard.entries.is_empty() {
        return None;
    }
    let mut entries = std::mem::take(&mut guard.entries);
    drop(guard);
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.total_us));
    let top = entries
        .into_iter()
        .take(4)
        .map(|entry| {
            format!(
                "{} {} calls {}ms (max {}ms)",
                entry.label,
                entry.calls,
                entry.total_us / 1000,
                entry.max_us / 1000
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(top)
}
