//! Startup phase timing instrumentation (pir equivalent of upstream
//! `core/timings.ts` + the `PI_STARTUP_BENCHMARK` gate from `main.ts`).
//!
//! Upstream gates the timing instrumentation itself on `PI_TIMING=1`
//! (core/timings.ts:6) and uses `PI_STARTUP_BENCHMARK` only for the
//! interactive init-then-stop cycle, erroring
//! `Error: PI_STARTUP_BENCHMARK only supports interactive mode` on any other
//! mode (main.ts:932-934). pir instruments the shared startup path in
//! `main_cli` and prints the same report shape to stderr; either env gate
//! enables the instrumentation/print, and the non-interactive benchmark
//! error is enforced in `run_mode`. The init-TUI-then-stop cycle itself is
//! not portable without touching the TUI host, so the benchmark reports the
//! pre-mode phases instead of running the interactive cycle.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The benchmark opt-in switch (upstream `main.ts` `PI_STARTUP_BENCHMARK`).
pub const ENV_STARTUP_BENCHMARK: &str = "PI_STARTUP_BENCHMARK";

/// The timing instrumentation gate (upstream `core/timings.ts:6`
/// `PI_TIMING`, strict `"1"`).
pub const ENV_TIMING: &str = "PI_TIMING";

/// Upstream `isTruthyEnvFlag`: `1`, `true`, or `yes` (case-insensitive).
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    match value {
        Some(value) => {
            let value = value.trim();
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
        None => false,
    }
}

/// Whether the startup benchmark is enabled for this process (upstream
/// `main.ts:932`).
pub fn startup_benchmark_enabled() -> bool {
    is_truthy_env_flag(std::env::var(ENV_STARTUP_BENCHMARK).ok().as_deref())
}

/// Whether the timing instrumentation is enabled (upstream
/// `core/timings.ts:6`: `PI_TIMING === "1"`).
pub fn timing_enabled() -> bool {
    std::env::var(ENV_TIMING).ok().as_deref() == Some("1")
}

/// Cached [`timing_enabled`] for per-request/per-event hot paths.
///
/// `timing_enabled` reads the process environment (an allocation per call), so
/// it must not sit on the token-stream or event-dispatch path. The gate is a
/// process-wide constant for the lifetime of a run; tests that need to flip it
/// use [`StartupTimings::with_enabled`] or a fresh subprocess.
pub fn timing_enabled_cached() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(timing_enabled)
}

/// Start a phase measurement when [`timing_enabled_cached`] is on; `None`
/// otherwise (the caller passes it to [`finish_phase`]).
pub fn start_phase() -> Option<Instant> {
    timing_enabled_cached().then(Instant::now)
}

/// Elapsed milliseconds of `start` (a no-op for a disabled phase).
pub fn finish_phase(start: Option<Instant>) -> Option<u128> {
    start.map(|start| start.elapsed().as_millis())
}

/// Log one phase duration to stderr when `PI_TIMING=1`:
/// `pir-timing: <label>: <ms>ms`. Never logs prompts, payloads or secrets —
/// labels are static phase names.
pub fn log_phase(label: &str, elapsed: Duration) {
    if timing_enabled_cached() {
        eprintln!("pir-timing: {label}: {}ms", elapsed.as_millis());
    }
}

/// [`log_phase`] at most once per process for `key` (which may differ from the
/// emitted `label` when one phase has several call sites).
pub fn log_phase_once(key: &str, label: &str, elapsed: Duration) {
    if !timing_enabled_cached() {
        return;
    }
    static LOGGED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let logged = LOGGED.get_or_init(|| Mutex::new(BTreeSet::new()));
    let mut guard = logged
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.insert(key.to_owned()) {
        log_phase(label, elapsed);
    }
}

/// Emit an already-measured elapsed value under `label`, once per `key`
/// (`None` — the disabled phase — is a no-op).
pub fn log_phase_once_opt(key: &str, label: &str, elapsed: Option<u128>) {
    if let Some(elapsed) = elapsed {
        log_phase_once(key, label, Duration::from_millis(elapsed as u64));
    }
}

/// Accumulates per-phase durations between [`StartupTimings::time`] calls.
#[derive(Debug)]
pub struct StartupTimings {
    enabled: bool,
    last: Option<Instant>,
    entries: Vec<(String, u128)>,
}

impl Default for StartupTimings {
    fn default() -> Self {
        Self::new()
    }
}

impl StartupTimings {
    /// Start measuring; enabled when either upstream gate is set —
    /// [`ENV_STARTUP_BENCHMARK`] truthy or [`ENV_TIMING=1`](ENV_TIMING) —
    /// so disabled runs cost one boolean check per phase.
    pub fn new() -> Self {
        Self::with_enabled(startup_benchmark_enabled() || timing_enabled())
    }

    /// Start measuring with an explicit on/off (tests and embedders).
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            last: Some(Instant::now()),
            entries: Vec::new(),
        }
    }

    /// Whether the benchmark is on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The recorded `(label, ms)` phases in call order.
    pub fn entries(&self) -> &[(String, u128)] {
        &self.entries
    }

    /// Record the duration since the previous `time` call (or construction)
    /// under `label` (upstream `time(label)`).
    pub fn time(&mut self, label: &str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let elapsed = self
            .last
            .replace(now)
            .map_or(0, |last| now.duration_since(last).as_millis());
        self.entries.push((label.to_string(), elapsed));
    }

    /// Reset the measurement origin WITHOUT recording a phase: the next
    /// [`Self::time`] measures from here (used to skip an idle stretch —
    /// e.g. the whole TUI session — between two reported phases).
    pub fn mark(&mut self) {
        if self.enabled {
            self.last = Some(Instant::now());
        }
    }

    /// Print the report to stderr (upstream `printTimings` format):
    /// `--- Startup Timings: main ---`, one `  <label>: <ms>ms` line each,
    /// a `  TOTAL:` line, and a matching dash rule.
    pub fn print_to_stderr(&self) {
        self.print_to_stderr_titled("Startup Timings: main");
    }

    /// [`Self::print_to_stderr`] with an explicit report title (the TUI
    /// host prints its own phase report after the terminal is restored).
    pub fn print_to_stderr_titled(&self, title: &str) {
        if !self.enabled || self.entries.is_empty() {
            return;
        }
        eprintln!("\n--- {title} ---");
        for (label, ms) in &self.entries {
            eprintln!("  {label}: {ms}ms");
        }
        let total: u128 = self.entries.iter().map(|(_, ms)| ms).sum();
        eprintln!("  TOTAL: {total}ms");
        eprintln!("{}", "-".repeat(title.len() + 8));
    }
}
