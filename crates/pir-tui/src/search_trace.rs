//! Env-gated latency trace for the alt-screen search/render pipeline
//! (PIR-TUI-011 quantification).
//!
//! `PIR_TUI_SEARCH_TRACE=1` appends one tab-separated line per measured
//! segment to `PIR_TUI_SEARCH_TRACE_FILE` (default `pir-search-trace.log` in
//! the process working directory). Disabled by default: the sink is resolved
//! once and every call site pays one `OnceLock` read plus, when disabled, a
//! branch. No behavior change when off.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

fn sink() -> &'static Option<Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        std::env::var_os("PIR_TUI_SEARCH_TRACE")?;
        let path = std::env::var("PIR_TUI_SEARCH_TRACE_FILE")
            .unwrap_or_else(|_| "pir-search-trace.log".to_string());
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Some(Mutex::new(file)),
            Err(_) => None,
        }
    })
}

/// Whether the trace is active (call sites can skip building detail strings).
pub fn enabled() -> bool {
    sink().is_some()
}

/// Append one event line: `event<TAB>field=value...` before the timestamp.
pub fn log(event: &str, fields: &[(&str, u128)]) {
    let Some(sink) = sink() else { return };
    let Ok(mut file) = sink.lock() else { return };
    let mut line = String::with_capacity(64 + fields.len() * 16);
    line.push_str(event);
    for (name, value) in fields {
        line.push('\t');
        line.push_str(name);
        line.push('=');
        line.push_str(&value.to_string());
    }
    line.push('\n');
    let _ = file.write_all(line.as_bytes());
}

/// Scope timer: `Scope::new("render")` on entry, [`Scope::finish`] at the exit
/// the caller wants measured. Logged in microseconds.
pub struct Scope {
    event: &'static str,
    start: Instant,
}

impl Scope {
    pub fn new(event: &'static str) -> Self {
        Self {
            event,
            start: Instant::now(),
        }
    }

    pub fn elapsed_us(&self) -> u128 {
        self.start.elapsed().as_micros()
    }

    #[allow(dead_code)]
    pub fn finish(self, fields: &[(&str, u128)]) {
        let mut all = Vec::with_capacity(fields.len() + 1);
        all.extend_from_slice(fields);
        log(self.event, &all);
    }
}

/// One-shot timer that logs when the measured block ends; call
/// [`Timer::log`] explicitly (function-wide guards with early returns still
/// work because logging is explicit, not `Drop`-based).
pub struct Timer {
    start: Option<Instant>,
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

impl Timer {
    pub fn new() -> Self {
        Self {
            start: enabled().then(Instant::now),
        }
    }

    pub fn elapsed_us(&self) -> u128 {
        self.start
            .map(|start| start.elapsed().as_micros())
            .unwrap_or(0)
    }

    pub fn log(&self, event: &str, fields: &[(&str, u128)]) {
        if self.start.is_some() {
            let mut all = Vec::with_capacity(fields.len() + 1);
            all.extend_from_slice(fields);
            all.push(("us", self.elapsed_us()));
            log(event, &all);
        }
    }
}
