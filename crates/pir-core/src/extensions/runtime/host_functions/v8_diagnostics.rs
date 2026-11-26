//! Synchronous QuickJS diagnostics for the `node:v8` and `node:vm` shims.

use std::cell::Cell;
use std::rc::Rc;

use rquickjs::object::Accessor;
use rquickjs::prelude::Func;
use rquickjs::{qjs, Ctx, Object, Result};

pub(super) fn register_v8_diagnostics(host: &Object<'_>) -> Result<()> {
    host.prop("v8HeapStatistics", Func::from(heap_statistics))?;

    // Per-runtime opt-in shared by the two shims, with no public global `gc`.
    // The collector itself also checks the flag; just reading this state never
    // touches QuickJS's collector or walks the heap.
    let expose_gc = Rc::new(Cell::new(false));
    let enable_gc = Rc::clone(&expose_gc);
    host.prop("enableVmGc", Func::from(move || enable_gc.set(true)))?;
    let gc_enabled = Rc::clone(&expose_gc);
    host.prop("vmGcEnabled", Accessor::from(move || gc_enabled.get()))?;
    host.prop(
        "runVmGc",
        Func::from(move |ctx: Ctx<'_>| {
            if expose_gc.get() {
                ctx.run_gc();
            }
        }),
    )?;
    Ok(())
}

fn heap_statistics<'js>(ctx: Ctx<'js>) -> Result<Object<'js>> {
    // The diagnostic is an on-demand walk. Sample before allocating the JS
    // result; do not reacquire the runtime lock or run GC to take a sample.
    let usage = memory_usage(&ctx);
    let result = Object::new(ctx)?;
    result.prop("mallocSize", usage.malloc_size.max(0) as f64)?;
    result.prop("memoryUsedSize", usage.memory_used_size.max(0) as f64)?;
    result.prop("mallocLimit", usage.malloc_limit.max(0) as f64)?;
    Ok(result)
}

#[allow(unsafe_code)]
fn memory_usage(ctx: &Ctx<'_>) -> qjs::JSMemoryUsage {
    let mut usage = std::mem::MaybeUninit::uninit();
    // SAFETY: rquickjs supplies a live Ctx while executing the synchronous
    // callback, with exclusive runtime entry already established. The derived
    // runtime pointer is used only here, without locking, JS re-entry or GC.
    // JS_ComputeMemoryUsage initializes the entire JSMemoryUsage out buffer
    // (it starts with memset) and retains neither pointer.
    unsafe {
        let runtime = qjs::JS_GetRuntime(ctx.as_raw().as_ptr());
        qjs::JS_ComputeMemoryUsage(runtime, usage.as_mut_ptr());
        usage.assume_init()
    }
}
