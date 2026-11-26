//! Typed C-ABI entry for the `path` kernels.
//!
//! `__pirNode.invoke` converts a live payload into a `serde_json::Value` tree
//! (map + string allocations) on every call; for `path.join("/a","b")` that
//! conversion dominates the actual work. This entry takes the same arguments
//! as live values and calls `pir-node`'s kernels directly, so the hot
//! `path.join`/`resolve`/`normalize`/`relative` calls cross the boundary with
//! strings only. The JS shim keeps its `__pirNode.invoke` and pure-JS
//! fallbacks, so a boot path without this global still works.
//!
//! Op/flavor travel as small integers: a JS string argument would be converted
//! to a Rust `String` (allocate, copy, free) before we could match on it.
//! `join`/`normalize` never consult the runtime at all — no `NodeState` lookup,
//! no `Arc` clone and, crucially, no cwd lock + `PathBuf` clone; only
//! `resolve`/`relative` need the cwd.

use rquickjs::prelude::Opt;
use rquickjs::{CString, Ctx, Result, Value};

use super::NodeState;

/// Must stay in sync with `pirPathKernel` in `path_shim.js.txt`.
const OP_JOIN: u32 = 0;
const OP_RESOLVE: u32 = 1;
const OP_NORMALIZE: u32 = 2;
const OP_RELATIVE: u32 = 3;

const FLAVOR_WIN32: u32 = 0;
const FLAVOR_POSIX: u32 = 1;

/// `CString` borrows QuickJS's UTF-8 view (for an 8-bit string this is the
/// string's own bytes) instead of copying into a Rust `String` like `FromJs
/// for String` does. Only the refcount moves.
fn cstring_arg<'js>(value: &Option<Value<'js>>) -> Option<CString<'js>> {
    CString::from_string(value.as_ref()?.as_string()?.clone()).ok()
}

fn cstring_list<'js>(value: &Option<Value<'js>>) -> Option<Vec<CString<'js>>> {
    let array = value.as_ref()?.as_array()?;
    let mut parts = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        parts.push(array.get::<CString>(index).ok()?);
    }
    Some(parts)
}

/// `ctx.userdata::<NodeState>()` is only touched by the ops that need a cwd.
fn current_cwd(ctx: &Ctx<'_>) -> Option<String> {
    let state = ctx.userdata::<NodeState>()?;
    let path = state.0.context.cwd();
    Some(
        path.into_os_string()
            .into_string()
            .unwrap_or_else(|os| os.to_string_lossy().into_owned()),
    )
}

/// `__pirPath(op, flavor, a[, b[, cwd]])`, op/flavor as integers (see the JS
/// shim):
/// * `normalize`: `a` is the path string.
/// * `relative`: `a`/`b` are `from`/`to`.
/// * `join`/`resolve`: `a` is an array of strings.
/// * `resolve`/`relative`: `c` is the cwd (the shim's `process.cwd()`, which is
///   memoized because `process.chdir` is unsupported). Omitted -> the runtime
///   state is consulted, preserving the old contract.
///
/// Returns the resulting string, or `undefined` when the shape does not match
/// (the JS tier then validates and runs its own implementation).
pub(super) fn path_native<'js>(
    ctx: Ctx<'js>,
    op: Value<'js>,
    flavor: Value<'js>,
    a: Opt<Value<'js>>,
    b: Opt<Value<'js>>,
    c: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    // Coerce instead of declaring `u32`: a caller that still passes the old
    // string signature (a stale AOT shim blob, a third-party probe) must get
    // the documented `undefined` fallback, not an argument-conversion
    // `TypeError` thrown from the binding.
    let (Some(op), Some(flavor)) = (op.as_number(), flavor.as_number()) else {
        return Ok(Value::new_undefined(ctx));
    };
    let (op, flavor) = (op as u32, flavor as u32);
    let flavor = match flavor {
        FLAVOR_WIN32 => crate::pir_node::modules::path::Flavor::Win32,
        FLAVOR_POSIX => crate::pir_node::modules::path::Flavor::Posix,
        _ => return Ok(Value::new_undefined(ctx)),
    };

    let result: Option<String> = match op {
        OP_NORMALIZE => cstring_arg(&a)
            .map(|path| crate::pir_node::modules::path::normalize(flavor, path.as_str())),
        OP_JOIN => {
            cstring_list(&a).map(|parts| crate::pir_node::modules::path::join(flavor, &parts))
        }
        OP_RESOLVE => {
            let provided = cstring_arg(&c);
            let fallback = if provided.is_none() {
                current_cwd(&ctx)
            } else {
                None
            };
            let cwd = provided
                .as_ref()
                .map(CString::as_str)
                .or(fallback.as_deref());
            match (cwd, cstring_list(&a)) {
                (Some(cwd), Some(parts)) => {
                    Some(crate::pir_node::modules::path::resolve(flavor, &parts, cwd))
                }
                _ => None,
            }
        }
        OP_RELATIVE => {
            let provided = cstring_arg(&c);
            let fallback = if provided.is_none() {
                current_cwd(&ctx)
            } else {
                None
            };
            let cwd = provided
                .as_ref()
                .map(CString::as_str)
                .or(fallback.as_deref());
            match (cstring_arg(&a), cstring_arg(&b), cwd) {
                (Some(from), Some(to), Some(cwd)) => {
                    Some(crate::pir_node::modules::path::relative(
                        flavor,
                        from.as_str(),
                        to.as_str(),
                        cwd,
                    ))
                }
                _ => None,
            }
        }
        _ => None,
    };

    match result {
        Some(text) => rquickjs::IntoJs::into_js(text, &ctx),
        None => Ok(Value::new_undefined(ctx)),
    }
}

/// Install `__pirPath` on the globals.
pub(super) fn register_path_ops(ctx: &Ctx<'_>) -> Result<()> {
    ctx.globals()
        .set("__pirPath", rquickjs::prelude::Func::from(path_native))
}
