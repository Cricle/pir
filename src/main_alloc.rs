// PIR-TUI-020: the `pir` binary is the only artifact allowed to install a
// process-wide allocator. A `#[global_allocator]` declared in the *library*
// replaces the allocator for every final artifact that links `pir`, which
// conflicts with any test binary or SDK embedder that registers its own
// counting allocator (zz_merged_1/zz6, mimalloc/jemalloc hosts). The
// instrumented allocator itself stays in the library (`tui::alloc_trace`);
// only the registration moved here.

/// Instrumented global allocator (PIR-TUI-019): delegates to `System` and
/// records allocation bytes only when `PIR_ALLOC_TRACE` is set. Off by
/// default — when unset, the hooks are two relaxed atomic loads per
/// allocation, no locks and no I/O.
#[cfg(feature = "tui")]
#[global_allocator]
static PIR_ALLOC_TRACE: pir::tui::alloc_trace::TracingAllocator =
    pir::tui::alloc_trace::TracingAllocator;
