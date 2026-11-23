//! Typed C-ABI entries for the metadata-shaped `fs`/`process` calls.
//!
//! `fs.statSync`/`readdirSync`/`existsSync` and `process.cwd()` are called in
//! plugin tight loops (directory walks, `exists` probes, per-render `cwd`
//! reads) and each one currently pays a full `__pirNodeCall` envelope: build a
//! payload object in JS, stringify it, parse it in Rust, build a
//! `serde_json::Value`, stringify that, parse it back in JS. These entries
//! pass the one string argument directly and answer with an already-shaped
//! live value, so only the syscall and the object construction remain.
//!
//! Contract, same as [`super::bytes_ops`]: a shape this tier does not own
//! answers `undefined` so the shim runs its envelope path (and its exact error
//! text); an accepted shape that fails *throws* the Node-shaped error.

use rquickjs::prelude::Opt;
use rquickjs::{Array, BigInt, Ctx, Object, Result, Value};

use super::bytes_ops::{raise, runtime_of, string_arg};

fn undefined<'js>(ctx: Ctx<'js>) -> Result<Value<'js>> {
    Ok(Value::new_undefined(ctx))
}

/// JS string/number `flag` -> the typed write flag. Returning `undefined` from
/// the entry means the shape is not owned here (the shim then runs its JSON
/// path, which validates too).
enum FlagArg {
    Text(String),
    Bits(i64),
}

/// Borrow a `Uint8Array` argument's bytes (the same contract as
/// `bytes_ops::fs_write`: Buffer instances are real `Uint8Array`s here).
#[allow(unsafe_code)]
fn view_bytes<'a, 'js>(value: &'a Value<'js>) -> Option<&'a [u8]> {
    let typed = value.as_object()?.as_typed_array::<u8>()?;
    // SAFETY: live attached typed array; no JS runs while the borrow is held.
    unsafe { typed.as_bytes() }
}

/// `__pirFsStat(path, lstat?)` -> the full `Stats` raw payload (booleans,
/// `mode`, exact integer fields as BigInt, `atimeNs`… as BigInt and the four
/// `*Ms` values as numbers), or `undefined` when `path` is not a string.
///
/// The exact integers cross as BigInt so `{ bigint: true }` never rounds them
/// through an f64. (On win32 the stable `MetadataExt` cannot supply the volume
/// serial / file index, so those two report 0; see `fs::stat_parts_of`.)
pub(super) fn fs_stat<'js>(
    ctx: Ctx<'js>,
    path: Value<'js>,
    lstat: Opt<bool>,
) -> Result<Value<'js>> {
    let Some(path) = string_arg(&path) else {
        return undefined(ctx);
    };
    let Some(runtime) = runtime_of(&ctx) else {
        return undefined(ctx);
    };
    match crate::pir_node::modules::fs::stat_parts(&runtime, &path, lstat.0.unwrap_or(false)) {
        Ok(parts) => {
            let object = Object::new(ctx.clone())?;
            object.set("isFile", parts.is_file)?;
            object.set("isDirectory", parts.is_directory)?;
            object.set("isSymbolicLink", parts.is_symbolic_link)?;
            object.set("mode", parts.mode)?;
            object.set("size", BigInt::from_u64(ctx.clone(), parts.size)?)?;
            object.set("dev", BigInt::from_u64(ctx.clone(), parts.dev)?)?;
            object.set("ino", BigInt::from_u64(ctx.clone(), parts.ino)?)?;
            object.set("nlink", BigInt::from_u64(ctx.clone(), parts.nlink)?)?;
            object.set("uid", BigInt::from_u64(ctx.clone(), parts.uid as u64)?)?;
            object.set("gid", BigInt::from_u64(ctx.clone(), parts.gid as u64)?)?;
            object.set("rdev", BigInt::from_u64(ctx.clone(), parts.rdev)?)?;
            object.set("blksize", BigInt::from_u64(ctx.clone(), parts.blksize)?)?;
            object.set("blocks", BigInt::from_u64(ctx.clone(), parts.blocks)?)?;
            object.set("atimeMs", parts.atime_ms())?;
            object.set("mtimeMs", parts.mtime_ms())?;
            object.set("ctimeMs", parts.ctime_ms())?;
            object.set("birthtimeMs", parts.birthtime_ms())?;
            object.set("atimeNs", BigInt::from_i64(ctx.clone(), parts.atime_ns)?)?;
            object.set("mtimeNs", BigInt::from_i64(ctx.clone(), parts.mtime_ns)?)?;
            object.set("ctimeNs", BigInt::from_i64(ctx.clone(), parts.ctime_ns)?)?;
            object.set(
                "birthtimeNs",
                BigInt::from_i64(ctx.clone(), parts.birthtime_ns)?,
            )?;
            Ok(object.into_value())
        }
        Err(error) => Err(raise(&ctx, &error)),
    }
}

/// `__pirFsWriteFlags(path, bytes, flag, mode?)` -> byte count or `undefined`.
///
/// The flag-aware write entry: `flag` is Node's normalized `stringToFlags`
/// result (a flag string such as `"wx"`/`"r+"` or an int32 mask), so exclusive
/// create and no-truncate read/write writes keep the zero-copy byte path.
/// Shapes this tier does not own answer `undefined` with no side effect.
pub(super) fn fs_write_flags<'js>(
    ctx: Ctx<'js>,
    path: Value<'js>,
    data: Value<'js>,
    flag: Value<'js>,
    mode: Opt<f64>,
) -> Result<Value<'js>> {
    let (Some(path), Some(bytes)) = (string_arg(&path), view_bytes(&data)) else {
        return undefined(ctx);
    };
    let Some(runtime) = runtime_of(&ctx) else {
        return undefined(ctx);
    };
    let parsed = if let Some(text) = flag.as_string() {
        match text.to_string() {
            Ok(text) => FlagArg::Text(text),
            Err(_) => return undefined(ctx),
        }
    } else {
        match flag.as_number() {
            Some(number)
                if number.is_finite()
                    && number.fract() == 0.0
                    && number >= i32::MIN as f64
                    && number <= i32::MAX as f64 =>
            {
                FlagArg::Bits(number as i64)
            }
            _ => return undefined(ctx),
        }
    };
    let mode = mode.0.map(|value| value as u32).unwrap_or(0o666);
    let flag = match &parsed {
        FlagArg::Text(text) => crate::pir_node::modules::fs::WriteFlag::Text(text),
        FlagArg::Bits(bits) => crate::pir_node::modules::fs::WriteFlag::Bits(*bits),
    };
    match crate::pir_node::modules::fs::write_file_flags(&runtime, &path, bytes, &flag, mode) {
        Ok(()) => Ok(Value::new_float(ctx, bytes.len() as f64)),
        Err(error) => Err(raise(&ctx, &error)),
    }
}

/// `__pirFsExists(path)` -> boolean (never throws: Node's `existsSync`
/// answers `false` for anything it cannot resolve).
pub(super) fn fs_exists<'js>(ctx: Ctx<'js>, path: Value<'js>) -> Result<Value<'js>> {
    let Some(path) = string_arg(&path) else {
        return undefined(ctx);
    };
    let Some(runtime) = runtime_of(&ctx) else {
        return undefined(ctx);
    };
    Ok(Value::new_bool(
        ctx,
        crate::pir_node::modules::fs::exists(&runtime, &path),
    ))
}

/// `__pirFsReaddir(path, dirents?)` -> `Array<string>` or an array of the raw
/// dirent payloads (`{name, isFile, isDirectory, isSymbolicLink}`), matching
/// what the `readdir` op returns for the same flags.
pub(super) fn fs_readdir<'js>(
    ctx: Ctx<'js>,
    path: Value<'js>,
    dirents: Opt<bool>,
) -> Result<Value<'js>> {
    let Some(path) = string_arg(&path) else {
        return undefined(ctx);
    };
    let Some(runtime) = runtime_of(&ctx) else {
        return undefined(ctx);
    };
    let dirents = dirents.0.unwrap_or(false);
    match crate::pir_node::modules::fs::readdir_parts(&runtime, &path, dirents) {
        Ok(entries) => {
            let array = Array::new(ctx.clone())?;
            for (index, entry) in entries.into_iter().enumerate() {
                if dirents {
                    let object = Object::new(ctx.clone())?;
                    object.set("name", entry.name)?;
                    object.set("isFile", entry.is_file)?;
                    object.set("isDirectory", entry.is_directory)?;
                    object.set("isSymbolicLink", entry.is_symbolic_link)?;
                    array.set(index, object)?;
                } else {
                    array.set(index, entry.name)?;
                }
            }
            Ok(array.into_value())
        }
        Err(error) => Err(raise(&ctx, &error)),
    }
}

/// `__pirProcessCwd()` -> the runtime's cwd as a string, or `undefined` when
/// the runtime is not attached (the shim then falls back to the envelope).
///
/// The shim memoizes the answer and drops the memo on `process.chdir`; the
/// entry itself is one C call plus one string allocation.
pub(super) fn process_cwd<'js>(ctx: Ctx<'js>) -> Result<Value<'js>> {
    let Some(runtime) = runtime_of(&ctx) else {
        return undefined(ctx);
    };
    // The NodeRuntime cwd (not the host context's base cwd) so a prior
    // `process.chdir` is reflected by the memoized fast path.
    rquickjs::IntoJs::into_js(runtime.cwd().to_string_lossy().into_owned(), &ctx)
}

/// Install the typed metadata entries on the globals.
pub(super) fn register_system_ops(ctx: &Ctx<'_>) -> Result<()> {
    use rquickjs::prelude::Func;
    ctx.globals().set("__pirFsStat", Func::from(fs_stat))?;
    ctx.globals().set("__pirFsExists", Func::from(fs_exists))?;
    ctx.globals().set("__pirFsReaddir", Func::from(fs_readdir))?;
    ctx.globals().set("__pirFsWriteFlags", Func::from(fs_write_flags))?;
    ctx.globals().set("__pirProcessCwd", Func::from(process_cwd))?;
    Ok(())
}
