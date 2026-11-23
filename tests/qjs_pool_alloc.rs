#![cfg(feature = "extensions")]
// The allocator spike deliberately exercises raw pointers through the
// `Allocator` contract; those calls are confined to this test module.
#![allow(unsafe_code)]

//! Single-engine allocator spike: the QuickJS size-class pool allocator.
//!
//! Include the allocator module directly so tests can choose each runtime mode
//! and opt into its counters without mutating the process environment. Only
//! the parity test enables or resets the counters in this module instance.
//!
//! Run with:
//!
//! ```text
//! cargo test --features extensions --test zz_merged_4 qjs_pool_alloc::
//! ```

#[allow(unsafe_code)]
#[allow(dead_code)]
#[path = "../src/extensions/runtime/qjs_alloc.rs"]
mod qjs_alloc;

use std::ptr;

use qjs_alloc::{reset_stats, stats, AllocStats, PoolAllocator};
use rquickjs::allocator::Allocator;
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, CaughtError};

/// Exercises allocation paths QuickJS actually uses: object/array churn,
/// string concatenation, a TypedArray, JSON round-tripping, closures, and a
/// 1 MiB buffer (which must take the large/system path). The result is a
/// deterministic JSON string so the two runtimes can be compared byte-for-byte.
const SCRIPT: &str = r#"
(function () {
  const out = {};

  const object = { a: 1, b: [1, 2, 3], c: { d: "e" } };
  out.object = JSON.stringify(object);

  const array = [];
  for (let i = 0; i < 2000; i++) array.push((i * i) % 97);
  out.array = array.reduce((a, b) => a + b, 0);

  let text = "";
  for (let i = 0; i < 5000; i++) text += String.fromCharCode(65 + (i % 26));
  out.string = text.length + ":" + text.charCodeAt(4999);

  const typed = new Uint8Array(4096);
  for (let i = 0; i < typed.length; i++) typed[i] = (i * 31) & 255;
  out.typed = typed.reduce((a, b) => a + b, 0);

  const parsed = JSON.parse(JSON.stringify({ x: [1, 2, 3], y: "z" }));
  out.json = parsed.x[2] + parsed.y;

  function make(n) { return () => n; }
  let closure = 0;
  for (let i = 0; i < 100; i++) closure += make(i)();
  out.closure = closure;

  const big = new Uint8Array(1024 * 1024);
  for (let i = 0; i < big.length; i += 1024) big[i] = i & 255;
  let checksum = 0;
  for (let i = 0; i < big.length; i++) checksum = (checksum + big[i]) >>> 0;
  out.big = big.length + ":" + checksum;

  // Short-lived-object churn: the dominant pool workload.
  for (let i = 0; i < 20000; i++) {
    const tmp = { v: i, s: "x" + i, a: [i, i + 1] };
    if (tmp.v === -1) throw new Error("unreachable");
  }

  return JSON.stringify(out);
})()
"#;

/// Evaluate the corpus and return its JSON result.
async fn eval_json(runtime: &AsyncRuntime) -> String {
    let context = AsyncContext::full(runtime).await.expect("create context");
    context
        .with(|ctx| ctx.eval::<String, _>(SCRIPT).expect("script evaluates"))
        .await
}

/// Direct `Allocator` checks that a JS-only run cannot target precisely:
/// class boundaries, realloc moves in every direction, zero-size and NULL
/// semantics, alignment, and calloc zeroing.
fn direct_allocator_checks() {
    let mut allocator = PoolAllocator::new();

    unsafe {
        // small: 10 bytes lands in the 16-byte class
        let p = allocator.alloc(10);
        assert!(!p.is_null());
        assert_eq!(p as usize % 16, 0, "payload must be 16-byte aligned");
        assert!(PoolAllocator::usable_size(p) >= 10);
        for i in 0..10 {
            *p.add(i) = i as u8;
        }

        // 10 -> 100 crosses 16 -> 128
        let p = allocator.realloc(p, 100);
        assert!(!p.is_null());
        assert!(PoolAllocator::usable_size(p) >= 100);
        for i in 0..10 {
            assert_eq!(*p.add(i), i as u8, "small -> small realloc lost data");
        }

        // 100 -> 3000 crosses into the large/system path
        let p = allocator.realloc(p, 3000);
        assert!(!p.is_null());
        assert!(PoolAllocator::usable_size(p) >= 3000);
        for i in 0..10 {
            assert_eq!(*p.add(i), i as u8, "small -> large realloc lost data");
        }

        // large -> large grow
        let p = allocator.realloc(p, 8000);
        assert!(!p.is_null());
        assert!(PoolAllocator::usable_size(p) >= 8000);
        for i in 0..10 {
            assert_eq!(*p.add(i), i as u8, "large -> large realloc lost data");
        }

        // shrink keeps the block (and its contents) in place
        let p = allocator.realloc(p, 24);
        assert!(!p.is_null());
        assert!(PoolAllocator::usable_size(p) >= 24);
        for i in 0..10 {
            assert_eq!(*p.add(i), i as u8, "shrink realloc lost data");
        }
        allocator.dealloc(p);

        // NULL and zero-size semantics mirror rquickjs' RustAllocator:
        // calloc with a zero factor is NULL, alloc(0) returns a real block,
        // usable_size(NULL) is 0, and realloc(NULL, n) acts like alloc(n).
        assert!(allocator.calloc(0, 16).is_null());
        assert!(allocator.calloc(4, 0).is_null());
        assert_eq!(PoolAllocator::usable_size(ptr::null_mut()), 0);

        let zero = allocator.alloc(0);
        assert!(!zero.is_null(), "alloc(0) must return a usable block");
        assert!(PoolAllocator::usable_size(zero) >= 1);
        allocator.dealloc(zero);

        let zero_realloc = allocator.realloc(ptr::null_mut(), 0);
        assert!(!zero_realloc.is_null());
        allocator.dealloc(zero_realloc);

        // calloc must zero the whole usable region, including class padding.
        let zeroed = allocator.calloc(3, 17); // 51 bytes -> class 64
        assert!(!zeroed.is_null());
        let usable = PoolAllocator::usable_size(zeroed);
        assert!(usable >= 51);
        for i in 0..usable {
            assert_eq!(*zeroed.add(i), 0, "calloc left byte {i} uninitialised");
        }
        allocator.dealloc(zeroed);

        // A large block round-trips through the header and stays aligned.
        let big = allocator.alloc(1024 * 1024);
        assert!(!big.is_null());
        assert_eq!(big as usize % 16, 0);
        assert!(PoolAllocator::usable_size(big) >= 1024 * 1024);
        allocator.dealloc(big);
    }
}

#[test]
fn pool_allocator_matches_system_and_balances_stats() {
    reset_stats();
    direct_allocator_checks();
    assert_eq!(stats(), AllocStats::default());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread tokio runtime");

    runtime.block_on(async {
        let system_runtime = AsyncRuntime::new().expect("system runtime");
        let system_out = eval_json(&system_runtime).await;
        drop(system_runtime);
        assert!(!system_out.is_empty());

        let pool_runtime =
            AsyncRuntime::new_with_alloc(PoolAllocator::with_stats(true)).expect("pool runtime");
        let pool_out = eval_json(&pool_runtime).await;
        drop(pool_runtime);

        assert_eq!(
            pool_out, system_out,
            "pool and system runtimes must produce identical results"
        );

        // ---- Stats balance after the runtime (and its allocator) is gone. ----
        let snapshot = stats();
        assert!(snapshot.allocs > 0, "pool recorded no allocations");
        assert!(snapshot.frees > 0, "pool recorded no frees");
        assert!(
            snapshot.large > 0,
            "the 1 MiB Uint8Array should take the large/system path"
        );
        assert!(
            snapshot.allocs >= snapshot.frees,
            "frees ({}) exceed allocs ({})",
            snapshot.frees,
            snapshot.allocs
        );
        assert!(
            snapshot.alloc_bytes >= snapshot.in_use_bytes,
            "in-use bytes ({}) exceed cumulative alloc bytes ({})",
            snapshot.in_use_bytes,
            snapshot.alloc_bytes
        );
        // QuickJS frees its heap in JS_FreeRuntime; allow a small residue for
        // any allocation released by a different path in the shim layer.
        assert!(
            snapshot.in_use_bytes < 64 * 1024,
            "in-use bytes at teardown: {} (allocs={}, frees={})",
            snapshot.in_use_bytes,
            snapshot.allocs,
            snapshot.frees
        );
    });
}

async fn check_heap_cap(runtime: AsyncRuntime, mode: &str) {
    const CAP_BYTES: usize = 2 * 1024 * 1024;
    const RETAINED_BYTES: i32 = 1024 * 1024;
    const REQUEST_BYTES: i32 = 1536 * 1024;
    const ALLOCATE: &str = "new Uint8Array(1536 * 1024).byteLength";

    // Use the engine cap in both modes. qjs-ng 0.16.2 checks its per-runtime
    // malloc limit before invoking allocator hooks, even for a custom allocator.
    runtime.set_memory_limit(CAP_BYTES).await;
    // Leave cycle collection to the explicit run_gc below.
    runtime.set_gc_threshold(usize::MAX).await;
    let context = AsyncContext::full(&runtime).await.expect("create context");

    context
        .with(|ctx| {
            let len = ctx
                .eval::<i32, _>(
                    r#"
                    (() => {
                        const retained = { bytes: new Uint8Array(1024 * 1024) };
                        retained.self = retained;
                        globalThis.pirRetained = retained;
                        return retained.bytes.byteLength;
                    })()
                "#,
                )
                .catch(&ctx)
                .expect("1 MiB allocation should fit under the cap");
            assert_eq!(len, RETAINED_BYTES, "{mode}");

            // Each allocation is bounded and fits on its own, but not together.
            // The live set leaves enough room to construct the OOM exception.
            let error = ctx
                .eval::<i32, _>(ALLOCATE)
                .catch(&ctx)
                .expect_err("the additional 1.5 MiB should exceed the cap");
            let CaughtError::Exception(exception) = error else {
                panic!("{mode}: expected a JS out-of-memory exception, got {error:?}");
            };
            assert_eq!(
                exception.message().as_deref(),
                Some("out of memory"),
                "{mode}"
            );

            ctx.eval::<(), _>("globalThis.pirRetained = null;")
                .expect("release the root of the cycle");
        })
        .await;

    let before_gc = runtime.memory_usage().await.malloc_size;
    runtime.run_gc().await;
    let after_gc = runtime.memory_usage().await.malloc_size;
    assert!(
        before_gc - after_gc >= i64::from(RETAINED_BYTES),
        "{mode}: GC did not reclaim the cycle's buffer ({before_gc} -> {after_gc} bytes)"
    );

    // The previously rejected request now succeeds in the same context with
    // the same cap, not just after a reset or a switch to unlimited memory.
    let len = context
        .with(|ctx| {
            ctx.eval::<i32, _>(ALLOCATE)
                .catch(&ctx)
                .expect("allocation after GC")
        })
        .await;
    assert_eq!(len, REQUEST_BYTES, "{mode}");
}

#[tokio::test]
async fn heap_cap_is_enforced_in_both_allocator_modes() {
    check_heap_cap(AsyncRuntime::new().expect("system runtime"), "system").await;
    check_heap_cap(
        AsyncRuntime::new_with_alloc(PoolAllocator::new()).expect("pool runtime"),
        "pool",
    )
    .await;
}
