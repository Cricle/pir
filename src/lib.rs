//! Pi Rust (pir) — facade crate.
//!
//! The historical single-crate surface is preserved: `use pir::...` keeps
//! resolving for every module the engine owns, because this crate re-exports
//! `pir-core` at its root. The workspace is split by responsibility, not by
//! API:
//!
//! - `pir-core` — the UI-agnostic engine (agent loop, providers, sessions,
//!   tools, config, extension runtime, protocol). It has no terminal or
//!   clipboard dependency.
//! - `pir-tui` — the terminal front end. It depends on `pir-core` and is
//!   re-exported here as [`tui`] behind the `tui` feature.
//! - `pir-sdk` (this crate) — the single crate an embedder depends on. The
//!   `pir` binary and the `pir-minimal` profile also live here.
//!
//! Another front end (web, GUI, remote) is another crate depending on
//! `pir-core`; it does not touch this facade. The `pir::sdk` embedding API is
//! re-exported from the engine, with the terminal-only upstream parity
//! helpers added when `tui` is on.
//!
//! Design and acceptance criteria: `docs/architecture.md` and
//! `docs/FEATURES.md`.

#![deny(unsafe_code)]

// The engine's public surface, unchanged at the path level.
pub use pir_core::*;
// `#[macro_export]` diagnostics are also reachable as `pir::ext_diag!` (glob
// re-exports do not reliably cover the macro namespace for every caller).
pub use pir_core::{ext_diag, ext_diag_debug, ext_diag_fatal, ext_diag_info};

// The terminal front end, re-exported as `pir::tui`.
#[cfg(feature = "tui")]
pub use pir_tui as tui;

/// Coding-agent surface with the terminal-only modules folded back in, so
/// `pir::coding_agent::session_pick` / `::bug_report` keep their historical
/// paths. Everything else is the engine module.
pub mod coding_agent {
    pub use pir_core::coding_agent::*;

    #[cfg(feature = "tui")]
    pub use pir_tui::{bug_report, session_pick};
}

/// The curated embedding facade. The engine owns the types and builder; the
/// terminal parity helpers (`run_interactive_mode`, theme/keybinding
/// utilities) are added only when the TUI is compiled in.
#[cfg(feature = "sdk")]
pub mod sdk {
    pub use pir_core::sdk::*;

    #[cfg(feature = "tui")]
    pub use pir_tui::sdk_parity::*;
}
