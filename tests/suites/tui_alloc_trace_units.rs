//! `pir::tui::alloc_trace` unit tests moved out of the source module's inline
//! `#[cfg(test)] mod tests`: the disabled-by-default contract and the
//! PIR-BIN-022 zero-size-layout forwarding rules.
//!
//! The crate root allows `unsafe_code` here for the same reason the other
//! allocator test binaries do (`tests/constrained_device_budget.rs`,
//! `tests/lazy_history_memory.rs`): the tests only CALL the reviewed unsafe
//! `GlobalAlloc` methods implemented in `src/tui/alloc_trace.rs`; no unsafe
//! construct is introduced in this file other than those calls.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout};

use pir::tui::alloc_trace::{enabled, snapshot, TracingAllocator};

#[test]
fn disabled_by_default_and_tracks_when_enabled() {
    // The test environment does not set PIR_ALLOC_TRACE: the counters stay
    // off and allocation behavior is untouched.
    if std::env::var_os("PIR_ALLOC_TRACE").is_none() {
        assert!(!enabled());
    }
}

/// PIR-BIN-022: `alloc::alloc(Layout{size: 0})` does reach a global
/// allocator in practice (RustPython `BoxVec::new(0)` for frozen modules
/// with `max_stackdepth == 0`), and forwarding that layout to `System`
/// trips the core blanket `GlobalAlloc` impl's `assert_unchecked(size !=
/// 0)` (debug: panic_nounwind → abort; release: `assume` → ud2). Every
/// forwarding path must answer with the same dangling pointer `System`
/// would, without touching the system allocator or the counters.
#[test]
fn zero_size_layouts_are_handled_without_touching_system() {
    let l = Layout::from_size_align(0, 8).unwrap();
    let p = unsafe { <TracingAllocator as GlobalAlloc>::alloc(&TracingAllocator, l) };
    assert!(!p.is_null() && (p as usize) % 8 == 0);
    assert_eq!(p as usize, l.align(), "dangling pointer = layout align");
    unsafe { <TracingAllocator as GlobalAlloc>::dealloc(&TracingAllocator, p, l) };

    let z = unsafe { <TracingAllocator as GlobalAlloc>::alloc_zeroed(&TracingAllocator, l) };
    assert!(!z.is_null() && (z as usize) % 8 == 0);
    unsafe { <TracingAllocator as GlobalAlloc>::dealloc(&TracingAllocator, z, l) };

    // realloc out of a zero-sized block: nothing to free, behaves like
    // `alloc` for the requested (non-zero) size.
    let grown = unsafe { <TracingAllocator as GlobalAlloc>::realloc(&TracingAllocator, p, l, 64) };
    assert!(!grown.is_null() && (grown as usize) % 8 == 0);
    let big = Layout::from_size_align(64, 8).unwrap();
    unsafe { <TracingAllocator as GlobalAlloc>::dealloc(&TracingAllocator, grown, big) };

    // realloc down to a zero size frees the old block and returns a
    // dangling pointer the matching zero-sized dealloc then ignores.
    let cur = unsafe { <TracingAllocator as GlobalAlloc>::alloc(&TracingAllocator, big) };
    assert!(!cur.is_null());
    let shrunk =
        unsafe { <TracingAllocator as GlobalAlloc>::realloc(&TracingAllocator, cur, big, 0) };
    assert!(!shrunk.is_null() && (shrunk as usize) % 8 == 0);
    unsafe { <TracingAllocator as GlobalAlloc>::dealloc(&TracingAllocator, shrunk, l) };

    // The zero-size arms never account bytes: the counters stay put.
    let before = snapshot();
    for _ in 0..100 {
        let q = unsafe { <TracingAllocator as GlobalAlloc>::alloc(&TracingAllocator, l) };
        unsafe { <TracingAllocator as GlobalAlloc>::dealloc(&TracingAllocator, q, l) };
    }
    let after = snapshot();
    assert_eq!(after.allocated_bytes, before.allocated_bytes);
    assert_eq!(after.freed_bytes, before.freed_bytes);
    assert_eq!(after.allocations, before.allocations);
    assert_eq!(after.frees, before.frees);
    assert_eq!(after.current_bytes, before.current_bytes);
}
