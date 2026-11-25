//! Runtime-construction seam for the QuickJS allocator.
//!
//! `AsyncRuntime::new()` uses QuickJS's default (libc `malloc`/`free`)
//! allocator. A size-class pool allocator can be plugged in here with
//! `AsyncRuntime::new_with_alloc`, without touching any other runtime wiring.
//! Keeping the switch in one function makes the allocator A/B measurable:
//! `PIR_QJS_ALLOC=pool` selects the pool, anything else keeps the default.
//!
//! Performance work on this file must keep the default path bit-identical to
//! the pre-seam behaviour (no extra allocation, no extra thread).
//!
//! # Pool allocator
//!
//! QuickJS allocates many short-lived, small objects (properties, strings,
//! atoms) and a handful of large buffers (typed arrays, source text). The
//! pool services the small sizes from per-size-class free lists backed by
//! 64 KiB slabs and forwards everything above 2048 bytes to the system
//! allocator. Each allocation is preceded by a 16-byte header so that the
//! size-less `dealloc` callback can recover the class (and, for large blocks,
//! the layout size) without an external table.
//!
//! Every `PoolAllocator` owns its free lists and slabs, so each runtime
//! (including per-worker runtimes) gets an independent pool. Setting
//! `PIR_QJS_ALLOC_STATS=1` at construction opts a pool runtime into the relaxed
//! process-global counters and a snapshot on teardown. Otherwise the pool
//! does not touch the counters.
//!
//! The allocator interface is `unsafe` by contract, so this module is the one
//! reviewed `unsafe` island for the allocator work: every block carries a
//! `// SAFETY:` note, and the `unsafe impl` states the trait invariant.
#![allow(unsafe_code)]

use std::alloc::Layout;
use std::ffi::OsStr;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use rquickjs::allocator::Allocator;
use rquickjs::{AsyncRuntime, Result};

/// Payload capacities serviced by the pool, in bytes. A pooled block occupies
/// `capacity + HEADER_SIZE` bytes so that `usable_size` (the capacity) is the
/// number of bytes writable after the returned pointer. Anything larger than
/// the last capacity is a "large" allocation handled by the system allocator.
///
/// Every capacity is a multiple of `BLOCK_ALIGN`, so adding the 16-byte header
/// keeps every block size a multiple of `BLOCK_ALIGN` too.
const SIZE_CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
const CLASS_COUNT: usize = SIZE_CLASSES.len();
/// Header marker for a system-allocated ("large") block. `u16::MAX` cannot be
/// a valid pool class index.
const LARGE_CLASS: u16 = u16::MAX;
/// QuickJS only requires `usize` alignment, but a 16-byte header and 16-byte
/// class sizes keep the returned pointer at least double-word aligned, matching
/// the alignment `RustAllocator` provides (8 bytes).
const BLOCK_ALIGN: usize = 16;
/// Every allocation is preceded by this header. The payload is returned at
/// `base + HEADER_SIZE`, so the payload alignment equals `BLOCK_ALIGN`.
const HEADER_SIZE: usize = 16;
/// "PIQJ" — guards against a dealloc/realloc on a foreign pointer in debug
/// builds. Release builds trust the `Allocator` contract and only validate the
/// class index before touching a free list.
const MAGIC: u32 = 0x5049_514a;
/// Bytes requested from the system allocator at a time for one size class.
const SLAB_BYTES: usize = 64 * 1024;

/// Per-allocation header, read back from `ptr - HEADER_SIZE` by `dealloc`,
/// `realloc` and `usable_size`.
///
/// `size` is the *usable* size (the class size for pooled blocks, the
/// 16-byte-rounded request for large blocks), so `usable_size` never reports
/// less than the requested size.
#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: u32,
    class: u16,
    reserved: u16,
    size: usize,
}

// The header must fit in the fixed prefix; on 64-bit targets it is exactly 16
// bytes (4 + 2 + 2 + 8). The payload offset is always HEADER_SIZE regardless.
const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_SIZE);

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static LARGE_ALLOCS: AtomicU64 = AtomicU64::new(0);
static IN_USE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Process-global counters for opted-in pools. The fields are read independently
/// with relaxed ordering, which is sufficient for diagnostics and the A/B harness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AllocStats {
    /// Successful `alloc`/`calloc` calls (including the destination of a
    /// `realloc` that had to move).
    pub(crate) allocs: u64,
    /// Cumulative usable bytes handed out.
    pub(crate) alloc_bytes: u64,
    /// `dealloc` calls (including the old block of a moving `realloc`).
    pub(crate) frees: u64,
    /// Allocations that bypassed the pool (> 2048 bytes).
    pub(crate) large: u64,
    /// Cumulative usable bytes currently owned by QuickJS.
    pub(crate) in_use_bytes: u64,
}

/// Snapshot the pool counters.
pub(crate) fn stats() -> AllocStats {
    AllocStats {
        allocs: ALLOCS.load(Ordering::Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        frees: FREES.load(Ordering::Relaxed),
        large: LARGE_ALLOCS.load(Ordering::Relaxed),
        in_use_bytes: IN_USE_BYTES.load(Ordering::Relaxed),
    }
}

/// Hidden test entry: clear the process-global counters so a test can measure
/// one pool runtime in isolation. Never called on the default path.
#[doc(hidden)]
#[allow(dead_code)]
pub(crate) fn reset_stats() {
    ALLOCS.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    FREES.store(0, Ordering::Relaxed);
    LARGE_ALLOCS.store(0, Ordering::Relaxed);
    IN_USE_BYTES.store(0, Ordering::Relaxed);
}

/// A slab carved into fixed-size blocks. The layout is retained so `Drop` can
/// return the whole slab to the system allocator after the JS runtime has
/// freed every block.
struct Slab {
    ptr: *mut u8,
    layout: Layout,
}

/// Per-runtime size-class pool allocator.
///
/// Not `Send`/`Sync` by design: a QuickJS runtime (and its allocator) is
/// confined to one thread, and each worker runtime constructs its own pool.
pub struct PoolAllocator {
    /// Head of each class's singly-linked free list. The link lives in the
    /// block's *header* (first `usize` bytes) while the block is free, never in
    /// the payload: this works for the smallest class too and leaves a freed
    /// block's user bytes untouched until it is reused.
    free_lists: [*mut u8; CLASS_COUNT],
    slabs: Vec<Slab>,
    stats_enabled: bool,
}

impl PoolAllocator {
    /// Create an empty pool with statistics disabled. No system memory is
    /// touched until the first allocation.
    pub fn new() -> Self {
        Self::with_stats(false)
    }

    /// Choose whether to collect and report statistics for this pool's lifetime.
    pub(crate) fn with_stats(stats_enabled: bool) -> Self {
        Self {
            free_lists: [ptr::null_mut(); CLASS_COUNT],
            slabs: Vec::new(),
            stats_enabled,
        }
    }

    /// Allocate a fresh slab for `class`, thread every block but the first
    /// onto the class free list, and return the first block start.
    fn refill(&mut self, class: usize) -> *mut u8 {
        let block = block_size(class);
        let per_slab = (SLAB_BYTES / block).max(1);
        let slab_bytes = per_slab * block;
        let Some(layout) = Layout::from_size_align(slab_bytes, BLOCK_ALIGN).ok() else {
            return ptr::null_mut();
        };

        // SAFETY: `layout` was just built with a non-zero size and a valid
        // power-of-two alignment; the returned pointer (or null) is the start
        // of an uninitialised region of exactly `slab_bytes` bytes.
        let slab = unsafe { std::alloc::alloc(layout) };
        if slab.is_null() {
            return ptr::null_mut();
        }
        self.slabs.push(Slab { ptr: slab, layout });

        // SAFETY: every block `i * block` lies inside the freshly allocated
        // slab. The smallest block is `16 + HEADER_SIZE` = 32 bytes, so the
        // first `usize` is always writable for the free-list link. Writing the
        // link into the header area is sound because a free block is not
        // observable to QuickJS.
        unsafe {
            for i in 1..per_slab {
                let block_start = slab.add(i * block);
                block_start.cast::<*mut u8>().write(self.free_lists[class]);
                self.free_lists[class] = block_start;
            }
            slab
        }
    }

    /// Pop a block start for `class` (refilling the slab when the list is
    /// empty), stamp its header, and return the payload.
    fn alloc_pool(&mut self, class: usize) -> *mut u8 {
        let mut block_start = self.free_lists[class];
        if block_start.is_null() {
            block_start = self.refill(class);
            if block_start.is_null() {
                return ptr::null_mut();
            }
        } else {
            // SAFETY: a non-null free-list head is a block start previously
            // pushed by `dealloc`/`refill`; its first `usize` bytes hold the
            // next link, and the block is not reachable by QuickJS until we
            // return it.
            let next = unsafe { block_start.cast::<*mut u8>().read() };
            self.free_lists[class] = next;
        }

        // SAFETY: `block_start` is the 16-byte-aligned start of a
        // `block_size(class)` (capacity + HEADER_SIZE >= 32) byte block, so
        // it is writable for a `Header`. `size` records the payload capacity,
        // which is exactly the region available after the header.
        unsafe {
            block_start.cast::<Header>().write(Header {
                magic: MAGIC,
                class: class as u16,
                reserved: 0,
                size: SIZE_CLASSES[class],
            });
        }

        // SAFETY: the block reserves `SIZE_CLASSES[class]` bytes after the
        // header, so the payload offset stays inside the block and is 16-byte
        // aligned because the block start is.
        unsafe { block_start.add(HEADER_SIZE) }
    }

    /// Allocate a block larger than the biggest class from the system
    /// allocator, with the same header layout. `usable` is already 16-byte
    /// rounded up.
    fn alloc_large(&mut self, usable: usize) -> *mut u8 {
        let Some(total) = HEADER_SIZE.checked_add(usable) else {
            return ptr::null_mut();
        };
        let Some(layout) = Layout::from_size_align(total, BLOCK_ALIGN).ok() else {
            return ptr::null_mut();
        };

        // SAFETY: `layout` has a non-zero size (at least HEADER_SIZE) and a
        // valid alignment; the result is either null or a writable region of
        // `total` bytes.
        let base = unsafe { std::alloc::alloc(layout) };
        if base.is_null() {
            return ptr::null_mut();
        }

        // SAFETY: `base` is writable for `total` bytes, so the header fits at
        // offset 0 and the payload at `HEADER_SIZE` is in bounds and aligned
        // to `BLOCK_ALIGN` because `base` is.
        unsafe {
            base.cast::<Header>().write(Header {
                magic: MAGIC,
                class: LARGE_CLASS,
                reserved: 0,
                size: usable,
            });
            base.add(HEADER_SIZE)
        }
    }
}

impl Default for PoolAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PoolAllocator {
    fn drop(&mut self) {
        if self.stats_enabled {
            let snapshot = stats();
            eprintln!(
                "PIR_QJS_ALLOC stats allocs={} alloc_bytes={} frees={} large={} in_use_bytes={}",
                snapshot.allocs,
                snapshot.alloc_bytes,
                snapshot.frees,
                snapshot.large,
                snapshot.in_use_bytes,
            );
        }

        // SAFETY: every `Slab` was created by `refill` from `std::alloc::alloc`
        // with exactly this pointer/layout pair, and `drain` hands each one out
        // once. The JS runtime is gone by the time the allocator is dropped, so
        // no payload pointer into these slabs can still be live.
        unsafe {
            for slab in self.slabs.drain(..) {
                std::alloc::dealloc(slab.ptr, slab.layout);
            }
        }
    }
}

/// First size class that can hold `size`, or `None` for a large allocation.
fn class_index(size: usize) -> Option<usize> {
    if size <= SIZE_CLASSES[0] {
        return Some(0);
    }
    if size > SIZE_CLASSES[CLASS_COUNT - 1] {
        return None;
    }
    SIZE_CLASSES.iter().position(|&class| size <= class)
}

/// Physical block size for a class: its payload capacity plus the header.
const fn block_size(class: usize) -> usize {
    SIZE_CLASSES[class] + HEADER_SIZE
}

/// Round `size` up to `BLOCK_ALIGN`, reporting overflow instead of wrapping.
fn round_up(size: usize) -> Option<usize> {
    size.checked_add(BLOCK_ALIGN - 1)
        .map(|padded| padded & !(BLOCK_ALIGN - 1))
}

// SAFETY: `Allocator` requires returned pointers to be null or a writable
// region of at least the requested size aligned to `usize`, and `usable_size`
// to report the available size. This implementation returns either null (on
// overflow/allocation failure) or `base + HEADER_SIZE`, where `base` comes
// from `std::alloc::alloc` with `BLOCK_ALIGN` (16-byte) alignment and the
// stored usable size is always at least the request. `dealloc`/`realloc`
// accept only pointers produced by this allocator (trait contract); the header
// is validated in debug builds before use.
unsafe impl Allocator for PoolAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        let (ptr, usable, large) = match class_index(size) {
            Some(class) => (self.alloc_pool(class), SIZE_CLASSES[class], false),
            None => match round_up(size) {
                Some(usable) => (self.alloc_large(usable), usable, true),
                None => return ptr::null_mut(),
            },
        };

        if self.stats_enabled && !ptr.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(usable as u64, Ordering::Relaxed);
            IN_USE_BYTES.fetch_add(usable as u64, Ordering::Relaxed);
            if large {
                LARGE_ALLOCS.fetch_add(1, Ordering::Relaxed);
            }
        }
        ptr
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        // Match rquickjs' `RustAllocator`: a zero element count or zero element
        // size yields null rather than a zero-length allocation.
        if count == 0 || size == 0 {
            return ptr::null_mut();
        }
        let Some(total) = count.checked_mul(size) else {
            // `RustAllocator` panics on overflow; returning null is the
            // allocation-failure contract QuickJS understands (throws OOM)
            // and avoids unwinding across the extern "C" trampoline.
            return ptr::null_mut();
        };

        let ptr = self.alloc(total);
        if ptr.is_null() {
            return ptr;
        }

        // Same usable-size computation as `alloc`, so calloc zeroes the whole
        // region QuickJS may consider usable.
        let usable = match class_index(total) {
            Some(class) => SIZE_CLASSES[class],
            None => round_up(total).unwrap_or(total),
        };

        // SAFETY: `ptr` came from `alloc(total)`, so it is writable for at
        // least `usable >= total` bytes; write_bytes initialises exactly that
        // region.
        unsafe { ptr::write_bytes(ptr, 0, usable) };
        ptr
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }

        // SAFETY: the trait contract guarantees `ptr` came from this allocator,
        // so `ptr - HEADER_SIZE` is a live, aligned block start holding a
        // `Header`.
        let (block_start, header) = unsafe {
            let block_start = ptr.sub(HEADER_SIZE);
            (block_start, block_start.cast::<Header>().read())
        };
        debug_assert_eq!(
            header.magic, MAGIC,
            "qjs_alloc: dealloc called with a pointer not owned by this PoolAllocator"
        );

        if self.stats_enabled {
            FREES.fetch_add(1, Ordering::Relaxed);
            IN_USE_BYTES.fetch_sub(header.size as u64, Ordering::Relaxed);
        }

        if header.class == LARGE_CLASS {
            let Some(total) = HEADER_SIZE.checked_add(header.size) else {
                debug_assert!(false, "qjs_alloc: corrupt large-block size in header");
                return;
            };
            let Some(layout) = Layout::from_size_align(total, BLOCK_ALIGN).ok() else {
                debug_assert!(false, "qjs_alloc: corrupt large-block layout in header");
                return;
            };
            // SAFETY: a large block was allocated by `alloc_large` with exactly
            // `HEADER_SIZE + usable` bytes at `BLOCK_ALIGN` alignment; the
            // header carries that usable size.
            unsafe { std::alloc::dealloc(block_start, layout) };
        } else {
            let class = header.class as usize;
            if class >= CLASS_COUNT {
                debug_assert!(false, "qjs_alloc: corrupt class id in header");
                return;
            }
            // SAFETY: `block_start` is a block carved from a slab and is at
            // least 32 bytes, so the first `usize` is writable for the
            // free-list link; the block is no longer reachable by QuickJS
            // after this call.
            unsafe {
                block_start.cast::<*mut u8>().write(self.free_lists[class]);
            }
            self.free_lists[class] = block_start;
        }
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        // libc/QuickJS semantics: realloc(NULL, n) behaves like malloc(n).
        if ptr.is_null() {
            return self.alloc(new_size);
        }

        // SAFETY: the trait contract guarantees `ptr` came from this allocator
        // and is still live, so its header is valid.
        let header = unsafe { ptr.sub(HEADER_SIZE).cast::<Header>().read() };
        debug_assert_eq!(
            header.magic, MAGIC,
            "qjs_alloc: realloc called with a pointer not owned by this PoolAllocator"
        );

        // Shrinking (or same size) keeps the block where it is. The stored
        // class/usable size is deliberately left untouched: `dealloc` must
        // return the block to the class it actually came from, and
        // `usable_size` is only required to be >= the requested size.
        if new_size <= header.size {
            return ptr;
        }

        let new_ptr = self.alloc(new_size);
        if new_ptr.is_null() {
            // Failure leaves the original block valid, as `realloc` requires.
            return ptr::null_mut();
        }

        // SAFETY: the old allocation is valid for `header.size` bytes and the
        // new one for at least `new_size > header.size`, so the copy is in
        // bounds on both sides and they cannot overlap (the new block was just
        // carved from a different free-list entry or slab).
        unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, header.size) };

        // SAFETY: `ptr` is still the live old allocation returned by this
        // allocator; after the copy it may be released.
        unsafe { self.dealloc(ptr) };
        new_ptr
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        // The extern trampoline in rquickjs already maps null to 0; handle it
        // here too so direct test calls are safe.
        if ptr.is_null() {
            return 0;
        }

        // SAFETY: the trait contract guarantees `ptr` points to an allocation
        // of this allocator, so its header is live and readable.
        let header = unsafe { ptr.sub(HEADER_SIZE).cast::<Header>().read() };
        debug_assert_eq!(
            header.magic, MAGIC,
            "qjs_alloc: usable_size called with a pointer not owned by this PoolAllocator"
        );
        header.size
    }
}

/// Construct the async runtime with the configured allocator.
///
/// Despite rquickjs's stale `set_memory_limit` note, qjs-ng 0.16.2 checks the
/// engine heap cap before dispatching the allocation to the selected allocator.
pub(crate) fn new_runtime() -> Result<AsyncRuntime> {
    if pool_requested() {
        let stats_enabled =
            std::env::var_os("PIR_QJS_ALLOC_STATS").as_deref() == Some(OsStr::new("1"));
        AsyncRuntime::new_with_alloc(PoolAllocator::with_stats(stats_enabled))
    } else {
        // Default path: unchanged, no allocator wrapper, no extra allocation.
        AsyncRuntime::new()
    }
}

/// `PIR_QJS_ALLOC=pool` (case-insensitive) opts into the pool allocator; every
/// other value, including unset and `system`, keeps `AsyncRuntime::new()`.
fn pool_requested() -> bool {
    std::env::var_os("PIR_QJS_ALLOC")
        .as_deref()
        .and_then(OsStr::to_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("pool"))
}
