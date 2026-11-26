//! Debug diagnostics for the repair passes.
//!
//! The upstream builtin logged through the engine's routed `ext_diag_info!` /
//! `ext_diag!` macros. This crate must stay engine-free, so its debug lines go
//! to standard error through the thin helpers below. Every call site is gated
//! by the package's `debug` flag, so the default configuration produces no
//! output, and the repair results themselves never depend on logging.
//!
//! Host-routed logging (tracing levels, repeat collapsing, a capture sink for
//! tests) is deliberately deferred until the host adapter is wired: a future
//! adapter can replace these two functions with a sink injected at the plugin
//! boundary without touching the pure passes.

/// Emit one informational debug line (ts `logRepair` / grammar-pass debug).
pub fn info_line(line: &str) {
    eprintln!("{line}");
}

/// Emit one warning line (the config loader's unreadable-file diagnostics).
pub fn warn_line(line: &str) {
    eprintln!("{line}");
}

/// `eprintln!`-shaped informational diagnostic, gated by the caller's `debug`.
#[macro_export]
macro_rules! diag_info {
    ($($arg:tt)*) => {
        $crate::diag::info_line(&format!($($arg)*))
    };
}

/// `eprintln!`-shaped warning diagnostic, gated by the caller's condition.
#[macro_export]
macro_rules! diag_warn {
    ($($arg:tt)*) => {
        $crate::diag::warn_line(&format!($($arg)*))
    };
}
