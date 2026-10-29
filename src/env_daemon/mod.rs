//! pi-env protocol-v1 daemon, built into pir: `pir env-daemon serve --token <hex>`.
//!
//! This is the upstream `packages/env/daemon` Rust daemon (`pi-env`, 4,998 lines
//! across 16 files) ported essentially verbatim, so pir can be **both** ends of
//! the `@earendil-works/pi-env` wire: a client (the JS shim in
//! `src/extensions/api/js/pi_env_shim.js.txt`) and the daemon other clients
//! deploy. The wire contract is upstream `packages/env/docs/protocol.md`
//! (frame = `u32 len | u8 type | u32 id | u32 jsonLen | json | bytes`,
//! ops = hello / file family / listFiles / exec / watch / pty family,
//! 16 MiB frame cap, cancel + 5 s ping / 30 s silence rules); the behavioural
//! reference is `packages/env/docs/semantics.md`.
//!
//! Porting notes (deltas from upstream, all deliberate):
//!
//! - **Entry point**: upstream is a standalone binary (`pi-env serve --token
//!   <hex>`); pir exposes the same argv shape as a hidden subcommand
//!   (`pir env-daemon serve --token <hex>`, intercepted in
//!   `main_cli/run_mode.rs` before clap so it never shows in `--help`, exactly
//!   like the `__codemode-worker` precedent). The `serve` first-line sync
//!   marker, frame protocol and op set are unchanged.
//! - **Version**: `hello.version` reports pir's `CARGO_PKG_VERSION` instead of
//!   the npm package version upstream's `build.rs` bakes in. Nothing in the
//!   protocol depends on it (the documented contract is `protocol: 1` plus the
//!   `features` list, which is unchanged).
//! - **windows-sys 0.52**: pir's pinned windows-sys line (upstream uses 0.61).
//!   Only the `HANDLE` spelling differs (`isize` vs pointer); the API calls,
//!   feature gates and semantics are identical. Keeping 0.52 avoids a second
//!   `windows-sys` version in the tree.
//! - **Inline unit tests**: upstream ships in-file `cfg(test)` modules in
//!   `frame`/`decode`/`scan`/`list`/`window`/`sys::windows`; the pir hygiene
//!   gate forbids inline tests in `src/`, so those vectors live in
//!   `tests/env_daemon_protocol.rs` instead (translated, not dropped).
//! - Everything else — scheduling (worker pool + dedicated threads for
//!   exec/pty/watch), cancellation, output-queue ordering (`output.rs`),
//!   spill files, line scanning, `listFiles` ignore-crate semantics, the
//!   watch state machine, and the Unix PTY implementation — is upstream's
//!   code, unmodified apart from `crate::` → `crate::env_daemon::` paths.
//! - **Feature split**: the wire data plane — [`frame`] and [`decode`] — stays
//!   core so the remote-env client (`crate::env_client`) and the JS shim keep
//!   working in a build without the daemon. The daemon half (the server, its
//!   operation set and the platform layer, `daemon.rs` + the private modules)
//!   compiles only with the `env-daemon` feature, which implies `env` (its
//!   `notify`/`ignore` watch/list backend).

// The protocol/vector modules are `pub` so the integration tests in `tests/`
// can exercise the frame codec and the ported unit-test vectors directly
// (upstream keeps them as in-file `cfg(test)` modules, which the pir hygiene
// gate forbids in `src/`).
pub mod decode;
pub mod frame;

// The daemon half. `env-daemon` implies `env`, so `list`/`watch` (the
// `ignore` walker and the `notify` watcher) are always available here; an
// `env`-only build compiles neither the daemon nor its ops.
#[cfg(feature = "env-daemon")]
mod daemon;
#[cfg(feature = "env-daemon")]
pub mod errors;
#[cfg(feature = "env-daemon")]
mod exec;
#[cfg(feature = "env-daemon")]
mod fs;
#[cfg(feature = "env-daemon")]
pub mod list;
#[cfg(feature = "env-daemon")]
mod output;
#[cfg(feature = "env-daemon")]
mod pty;
#[cfg(feature = "env-daemon")]
pub mod scan;
#[cfg(feature = "env-daemon")]
pub mod sys;
#[cfg(feature = "env-daemon")]
mod watch;
#[cfg(feature = "env-daemon")]
pub mod window;

/// Resource-policy seams (worker-pool size, per-request stack) for the
/// low-resource lane's tests: the daemon's thread model is part of its
/// contract on 1-2 core / 512 MB devices.
#[cfg(feature = "env-daemon")]
#[doc(hidden)]
pub use daemon::{daemon_worker_count, daemon_worker_count_from, request_stack_bytes};
#[cfg(feature = "env-daemon")]
#[doc(hidden)]
pub use daemon::{max_long_requests, max_long_requests_from};
#[cfg(feature = "env-daemon")]
pub use daemon::{run_main, serve, ServeError};

// ---------------------------------------------------------------------------
// Process-wide resource budgets
//
// The daemon's own worker/stack policy lives in `daemon.rs` (only compiled
// with `env-daemon`). The policies below belong to EVERY `pir` process — the
// main runtime and the `pir js` script thread — so they live in this
// always-compiled module: the binary is built from `src/main.rs` (not from the
// library) and its integration tests can only reach library items.
// ---------------------------------------------------------------------------

/// Environment override for the main tokio runtime's worker-thread count
/// (`PIR_TOKIO_WORKERS`, 1..=64). Unset keeps tokio's own default (one worker
/// per core).
pub const TOKIO_WORKERS_ENV: &str = "PIR_TOKIO_WORKERS";

/// Environment override for the main runtime's blocking-pool ceiling
/// (`PIR_TOKIO_MAX_BLOCKING`, 1..=512).
pub const TOKIO_MAX_BLOCKING_ENV: &str = "PIR_TOKIO_MAX_BLOCKING";

/// Blocking threads kept even on a 1-core device: tool execution, process
/// spawning and session I/O are blocking work that must not queue behind one
/// another.
const MIN_BLOCKING_THREADS: usize = 4;

/// Blocking-pool ceiling. Tokio's default is 512 threads at 2 MiB of reserved
/// stack each (~1 GiB of address space) — far past what a phone or a 2-core
/// mini device needs.
const MAX_BLOCKING_THREADS: usize = 32;

/// The main runtime's blocking-pool ceiling: [`TOKIO_MAX_BLOCKING_ENV`] when
/// it parses (1..=512), otherwise two per core clamped to
/// `[MIN_BLOCKING_THREADS, MAX_BLOCKING_THREADS]`.
pub fn tokio_max_blocking_threads() -> usize {
    tokio_max_blocking_threads_from(
        std::env::var(TOKIO_MAX_BLOCKING_ENV).ok().as_deref(),
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1),
    )
}

/// The policy behind [`tokio_max_blocking_threads`]; unparseable or zero
/// overrides fall back to the adaptive decision.
#[doc(hidden)]
pub fn tokio_max_blocking_threads_from(override_value: Option<&str>, parallelism: usize) -> usize {
    if let Some(count) = override_value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
    {
        return count.clamp(1, 512);
    }
    parallelism
        .saturating_mul(2)
        .clamp(MIN_BLOCKING_THREADS, MAX_BLOCKING_THREADS)
}

/// The main runtime's worker-thread count: [`TOKIO_WORKERS_ENV`] when it
/// parses (1..=64), otherwise `None` (tokio's own one-per-core default).
pub fn tokio_worker_count() -> Option<usize> {
    tokio_worker_count_from(std::env::var(TOKIO_WORKERS_ENV).ok().as_deref())
}

/// The policy behind [`tokio_worker_count`].
#[doc(hidden)]
pub fn tokio_worker_count_from(override_value: Option<&str>) -> Option<usize> {
    override_value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .map(|count| count.clamp(1, 64))
}

/// Stack budget for the `pir` process's own deeply-recursive entry threads
/// (`pir-main` and the `pir js` script thread). Debug builds overflow the
/// 1 MiB Windows main-thread stack inside the clap tree and the swc/QuickJS
/// module compile; the release tier keeps the same headroom until the deep
/// markdown/render recursion is measured, so the seam exists but both tiers
/// agree today.
#[doc(hidden)]
pub fn stack_budget_bytes(debug: bool) -> usize {
    // Both tiers agree today; the parameter stays so the debug/release split
    // can diverge once the deep markdown/render recursion is measured.
    let _ = debug;
    STACK_BUDGET_BYTES
}

/// The release stack budget, for callers that want the value without a
/// branch (`stack_budget_bytes(cfg!(debug_assertions))` is the canonical
/// call).
#[doc(hidden)]
pub const STACK_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// The hidden subcommand pir answers as the pi-env daemon (upstream argv shape
/// preserved: `pir env-daemon serve --token <hex>`). Intercepted in
/// `main_cli/run_mode.rs` before clap parsing, so it stays out of `--help`,
/// like [`crate::plugins::codemode::worker::WORKER_SUBCOMMAND`].
///
/// Always compiled: a build without the `env-daemon` feature uses it to report
/// the documented missing-feature diagnostic instead of an unrecognized
/// subcommand.
// A lib-only build has no caller (the `pir` bin's pre-clap arm is the only
// one); gating the const would break that off-arm in a `cli`-only bin.
#[allow(
    dead_code,
    reason = "public surface for the `pir` bin's pre-clap `env-daemon` arm; not referenced inside the library"
)]
pub const DAEMON_SUBCOMMAND: &str = "env-daemon";
