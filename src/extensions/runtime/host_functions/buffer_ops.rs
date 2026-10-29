//! Native (rquickjs C-ABI) Buffer hot paths — the §6.5 "host-first" policy's
//! first landing batch.
//!
//! QuickJS executes the JS shim's numeric/codec methods as interpreted
//! bytecode; the functions here are direct C-ABI calls that read/write the
//! receiver's typed-array backing store and do the byte↔string codecs in
//! Rust, with no `__pir_host.node` JSON envelope in between.
//!
//! Install contract (JS side, see `extensions/api/js/buffer_global.js.txt`
//! and `textcodec_global.js.txt`):
//!
//! - `__pirInstallBufferOps(Buffer.prototype)` overwrites the JS DataView
//!   numeric methods (`readUInt8`…`swap64`) with native closures of identical
//!   semantics (Node's two-tier `ERR_OUT_OF_RANGE` / `ERR_BUFFER_OUT_OF_BOUNDS`
//!   `RangeError`s included). The variadic `readUIntLE/BE` · `writeUIntLE/BE` ·
//!   `readIntLE/BE` · `writeIntLE/BE` family stays on the JS implementation.
//! - `__pirBufferCodec(op, …)` is the encode/decode dispatcher behind the
//!   JS codec state machines (`utf8Bytes`, `hexEncode`, `base64Bytes`, …).
//!   String-input ops return `undefined` when the input cannot cross the
//!   host bridge (e.g. a lone surrogate, which `JS_ToCStringLen` cannot
//!   marshal losslessly) — the JS state machine is the automatic fallback,
//!   so observable behavior never changes.
//! - `__pirInstallTextCodec(TextEncoder.prototype, TextDecoder.prototype)`
//!   installs `__pirNativeEncode` / `__pirNativeEncodeInto` on the encoder
//!   prototype. The decoder prototype is intentionally untouched: decode
//!   keeps per-instance streaming state (`__pending`), which must live in
//!   the JS class; its `decode` calls the stateless chunk ops of
//!   `__pirBufferCodec` instead.
//! - `__pirIntlFormat(op, …)` backs the Intl pad's `NumberFormat.format`
//!   (grouped decimals) and `DateTimeFormat.format` (UTC ISO shape).
//! - `__pirBufferCodec("stringDecodeChunk", view, _, _, decoder)` and its
//!   `"stringDecodeFlush"` twin back `node:string_decoder`: the chunk op ports
//!   the shim's whole `write()` state machine (Node's `DecodeData` walk-back,
//!   maximal-subpart UTF-8, utf16le surrogate holding, base64 `nread % 3`),
//!   reading the decoder's `_pirPending`/`_pirNeed`/`_pirWindow` triple off
//!   the receiver and returning the new state. A `utf16le` result that would
//!   contain a lone surrogate (legal in a JS string, not in a Rust `String`)
//!   answers `undefined`, so the JS state machine takes that chunk over.
//! - `__pirBufferCodec("indexOfBytes", buffer, backwards, align2, needle,
//!   start)` backs the `indexOf`/`includes`/`lastIndexOf` scan loops of
//!   `buffer_global.js.txt` with a Rust single-byte / slice-compare scan; the
//!   JS loops stay as the fallback and keep Node's offset/needle/encoding
//!   resolution.
//! - `__pirBufferCodec("variadicWrite"/"variadicRead", buffer, bigEndian,
//!   signed, value, packed)` replaces the BigInt loop inside the 12 variadic
//!   `read/write[U]Int{LE,BE}` kernels (the methods keep their Node validation
//!   and error text in JS; `packed` is `offset * 8 + byteLength`).
//! - `__pirBufferCodec("strEscape", string)` is the `util.inspect` string
//!   escaper (`pirStrEscape`); a lone surrogate cannot cross the bridge, so
//!   those strings answer `undefined` and the JS tier stays in charge.
//!
//! Every JS consumer guards the install with `typeof globalThis.X ===
//! "function"` and keeps its JS implementation as the fallback, so any boot
//! path that does not register these globals (future embedders) degrades to
//! the pure-JS tier without code changes.

use std::borrow::Cow;

use rquickjs::prelude::{Func, Opt, This};
use rquickjs::{
    Array, BigInt, Coerced, Constructor, Ctx, Exception, FromJs, IntoJs, Object, Result, Type,
    TypedArray, Value,
};

/// Thrown when a numeric/codec method's receiver is not a typed array
/// (detached or a non-Buffer `this`); a should-never-happen path.
const RECEIVER_TYPE_MESSAGE: &str = "The \"this\" value must be a Buffer or Uint8Array";

/// The JS TextDecoder fatal-mode error (Web platform text).
const FATAL_DECODE_MESSAGE: &str = "The encoded data is not valid.";

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Registers the four plain globals this module installs. Called once per
/// runtime boot, before `node_shims::globals_source()` evaluates the JS
/// shims that consume them (see `runtime.rs`).
pub(super) fn register_buffer_ops(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();
    globals.set("__pirInstallBufferOps", Func::from(install_buffer_proto))?;
    globals.set("__pirBufferCodec", Func::from(buffer_codec_entry))?;
    globals.set("__pirInstallTextCodec", Func::from(install_text_codec))?;
    globals.set("__pirIntlFormat", Func::from(intl_format_entry))?;
    Ok(())
}

/// Installs the native numeric read/write/swap family onto `Buffer.prototype`
/// (direct assignment: native must win when present; the JS versions stay as
/// the fallback for boot paths without this global).
fn install_buffer_proto(proto: Object<'_>) -> Result<()> {
    // Each macro expands to a *named* generic fn (`fn op<'js>(…)`): fn items
    // are higher-ranked over 'js, which is what `Func::from` requires —
    // closure literals elide each parameter's lifetime independently and
    // fail the invariance check on `Value<'js>`.
    macro_rules! write_op {
        ($name:literal, $min:expr, $max:expr, $size:literal, $be:literal) => {{
            fn op<'js>(
                ctx: Ctx<'js>,
                this: This<Object<'js>>,
                value: Opt<Value<'js>>,
                offset: Opt<Value<'js>>,
            ) -> Result<Value<'js>> {
                let result = do_write_int(
                    &ctx,
                    &this.0,
                    value.0.as_ref(),
                    offset.0.as_ref(),
                    $min,
                    $max,
                    $size,
                    $be,
                )?;
                Ok(Value::new_float(ctx, result as f64))
            }
            proto.set($name, Func::from(op))?;
        }};
    }
    macro_rules! read_op {
        ($name:literal, $signed:literal, $size:literal, $be:literal) => {{
            fn op<'js>(
                ctx: Ctx<'js>,
                this: This<Object<'js>>,
                offset: Opt<Value<'js>>,
            ) -> Result<Value<'js>> {
                let result = do_read_int(&ctx, &this.0, offset.0.as_ref(), $signed, $size, $be)?;
                Ok(Value::new_float(ctx, result as f64))
            }
            proto.set($name, Func::from(op))?;
        }};
    }
    macro_rules! write_float_op {
        ($name:literal, $double:literal, $be:literal) => {{
            fn op<'js>(
                ctx: Ctx<'js>,
                this: This<Object<'js>>,
                value: Opt<Value<'js>>,
                offset: Opt<Value<'js>>,
            ) -> Result<Value<'js>> {
                let result = do_write_float(
                    &ctx,
                    &this.0,
                    value.0.as_ref(),
                    offset.0.as_ref(),
                    $double,
                    $be,
                )?;
                Ok(Value::new_float(ctx, result as f64))
            }
            proto.set($name, Func::from(op))?;
        }};
    }
    macro_rules! read_float_op {
        ($name:literal, $double:literal, $be:literal) => {{
            fn op<'js>(
                ctx: Ctx<'js>,
                this: This<Object<'js>>,
                offset: Opt<Value<'js>>,
            ) -> Result<Value<'js>> {
                let result = do_read_float(&ctx, &this.0, offset.0.as_ref(), $double, $be)?;
                Ok(Value::new_float(ctx, result))
            }
            proto.set($name, Func::from(op))?;
        }};
    }
    macro_rules! write_big_op {
        ($name:literal, $signed:literal, $be:literal) => {{
            fn op<'js>(
                ctx: Ctx<'js>,
                this: This<Object<'js>>,
                value: Opt<Value<'js>>,
                offset: Opt<Value<'js>>,
            ) -> Result<Value<'js>> {
                let result = do_write_big(
                    &ctx,
                    &this.0,
                    value.0.as_ref(),
                    offset.0.as_ref(),
                    $signed,
                    $be,
                )?;
                Ok(Value::new_float(ctx, result as f64))
            }
            proto.set($name, Func::from(op))?;
        }};
    }
    macro_rules! read_big_op {
        ($name:literal, $signed:literal, $be:literal) => {{
            fn op<'js>(
                ctx: Ctx<'js>,
                this: This<Object<'js>>,
                offset: Opt<Value<'js>>,
            ) -> Result<Value<'js>> {
                do_read_big(&ctx, &this.0, offset.0.as_ref(), $signed, $be)
            }
            proto.set($name, Func::from(op))?;
        }};
    }
    macro_rules! swap_op {
        ($name:literal, $size:literal) => {{
            fn op<'js>(ctx: Ctx<'js>, this: This<Object<'js>>) -> Result<Value<'js>> {
                do_swap(&ctx, &this.0, $size)
            }
            proto.set($name, Func::from(op))?;
        }};
    }

    // -- writes (value, offset) --------------------------------------------
    write_op!("writeUInt8", 0.0, 255.0, 1, false);
    write_op!("writeInt8", -128.0, 127.0, 1, false);
    write_op!("writeUInt16LE", 0.0, 65_535.0, 2, false);
    write_op!("writeUInt16BE", 0.0, 65_535.0, 2, true);
    write_op!("writeInt16LE", -32_768.0, 32_767.0, 2, false);
    write_op!("writeInt16BE", -32_768.0, 32_767.0, 2, true);
    write_op!("writeUInt32LE", 0.0, 4_294_967_295.0, 4, false);
    write_op!("writeUInt32BE", 0.0, 4_294_967_295.0, 4, true);
    write_op!("writeInt32LE", -2_147_483_648.0, 2_147_483_647.0, 4, false);
    write_op!("writeInt32BE", -2_147_483_648.0, 2_147_483_647.0, 4, true);

    // -- reads (offset) ------------------------------------------------------
    read_op!("readUInt8", false, 1, false);
    read_op!("readInt8", true, 1, false);
    read_op!("readUInt16LE", false, 2, false);
    read_op!("readUInt16BE", false, 2, true);
    read_op!("readInt16LE", true, 2, false);
    read_op!("readInt16BE", true, 2, true);
    read_op!("readUInt32LE", false, 4, false);
    read_op!("readUInt32BE", false, 4, true);
    read_op!("readInt32LE", true, 4, false);
    read_op!("readInt32BE", true, 4, true);

    // -- floats ---------------------------------------------------------------
    write_float_op!("writeFloatLE", false, false);
    write_float_op!("writeFloatBE", false, true);
    write_float_op!("writeDoubleLE", true, false);
    write_float_op!("writeDoubleBE", true, true);
    read_float_op!("readFloatLE", false, false);
    read_float_op!("readFloatBE", false, true);
    read_float_op!("readDoubleLE", true, false);
    read_float_op!("readDoubleBE", true, true);

    // -- 64-bit bigints -------------------------------------------------------
    write_big_op!("writeBigUInt64LE", false, false);
    write_big_op!("writeBigUInt64BE", false, true);
    write_big_op!("writeBigInt64LE", true, false);
    write_big_op!("writeBigInt64BE", true, true);
    read_big_op!("readBigUInt64LE", false, false);
    read_big_op!("readBigUInt64BE", false, true);
    read_big_op!("readBigInt64LE", true, false);
    read_big_op!("readBigInt64BE", true, true);

    // -- byte-order swaps ------------------------------------------------------
    swap_op!("swap16", 2);
    swap_op!("swap32", 4);
    swap_op!("swap64", 8);

    Ok(())
}

/// Installs native `encode`/`encodeInto` cores on `TextEncoder.prototype` as
/// advisory hooks (`__pirNativeEncode` / `__pirNativeEncodeInto`); the JS
/// class methods stay the entry points and fall back to their state machines
/// when a hook returns `undefined`. The decoder prototype is untouched (see
/// the module docs for why).
fn install_text_codec(encoder_proto: Object<'_>, _decoder_proto: Object<'_>) -> Result<()> {
    // Named generic fns, not closures: see the note in `install_buffer_proto`.
    fn op_encode<'js>(ctx: Ctx<'js>, input: Opt<Value<'js>>) -> Result<Value<'js>> {
        native_text_encode(&ctx, input.0.as_ref())
    }
    fn op_encode_into<'js>(
        ctx: Ctx<'js>,
        source: Opt<Value<'js>>,
        target: Opt<Value<'js>>,
    ) -> Result<Value<'js>> {
        native_text_encode_into(&ctx, source.0.as_ref(), target.0.as_ref())
    }
    encoder_proto.set("__pirNativeEncode", Func::from(op_encode))?;
    encoder_proto.set("__pirNativeEncodeInto", Func::from(op_encode_into))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// JS error construction (Node-shaped RangeError with a `code` property)
// ---------------------------------------------------------------------------

/// Throws a JS `RangeError` carrying `code` as an own property — the shape
/// the JS shim produces with `error.code = "ERR_…"`. Falls back to a bare
/// RangeError if the global constructor is unreachable.
fn throw_range_error(ctx: &Ctx<'_>, message: &str, code: &str) -> rquickjs::Error {
    let ctor: Result<Constructor> = ctx.globals().get("RangeError");
    if let Ok(ctor) = ctor {
        if let Ok(obj) = ctor.construct::<_, Object>((message,)) {
            let _ = obj.set("code", code);
            if let Some(exception) = Exception::from_object(obj) {
                return exception.throw();
            }
        }
    }
    Exception::throw_range(ctx, message)
}

/// TypeError twin of [`throw_range_error`] (`ERR_INVALID_ARG_TYPE` shapes).
fn throw_type_error(ctx: &Ctx<'_>, message: &str, code: &str) -> rquickjs::Error {
    let ctor: Result<Constructor> = ctx.globals().get("TypeError");
    if let Ok(ctor) = ctor {
        if let Ok(obj) = ctor.construct::<_, Object>((message,)) {
            let _ = obj.set("code", code);
            if let Some(exception) = Exception::from_object(obj) {
                return exception.throw();
            }
        }
    }
    Exception::throw_type(ctx, message)
}

/// JS `String(n)` parity for the error text (covers the NaN/±Infinity cases
/// Rust's `Display` renders differently), plus Node's `_` digit grouping for
/// magnitudes above 2^32 (its `formatValue` quirk, e.g.
/// `9_007_199_254_740_992` — the grouping walks the whole string, so signs,
/// decimals and exponents group too).
fn js_number_str(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value == f64::INFINITY {
        "Infinity".to_string()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else {
        let text = format!("{value}");
        if value.abs() > 4_294_967_296.0 {
            group_digits(&text)
        } else {
            text
        }
    }
}

/// Insert `_` every three characters from the right of the whole string.
fn group_digits(text: &str) -> String {
    // The grouping depends only on the distance to the end, i.e. on the
    // character count — no `Vec<char>` copy of the string to index into.
    let count = text.chars().count();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (i, c) in text.chars().enumerate() {
        if i > 0 && (count - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

fn offset_range_message(bound: i64, received: &str) -> String {
    format!("The value of \"offset\" is out of range. It must be >= 0 and <= {bound}. Received {received}")
}

/// Node/V8 keeps a dedicated message for a non-integer offset (no `>=`/`<=`
/// bounds in it).
fn offset_integer_message(received: &str) -> String {
    format!("The value of \"offset\" is out of range. It must be an integer. Received {received}")
}

fn offset_out_of_range(ctx: &Ctx<'_>, bound: i64, received: &str) -> rquickjs::Error {
    throw_range_error(
        ctx,
        &offset_range_message(bound, received),
        "ERR_OUT_OF_RANGE",
    )
}

/// Node's `boundsError` negative-length branch: a buffer shorter than the
/// access itself can never be in range, and the error is the dedicated
/// `ERR_BUFFER_OUT_OF_BOUNDS` ("Attempt to access memory outside buffer
/// bounds"), not an out-of-range offset message.
fn buffer_out_of_bounds_access(ctx: &Ctx<'_>) -> rquickjs::Error {
    throw_range_error(
        ctx,
        "Attempt to access memory outside buffer bounds",
        "ERR_BUFFER_OUT_OF_BOUNDS",
    )
}

/// Node's `ERR_INVALID_ARG_TYPE` text for a non-number numeric argument
/// (`The "offset" argument must be of type number. Received …`).
fn invalid_arg_type(ctx: &Ctx<'_>, name: &str, value: &Value<'_>) -> rquickjs::Error {
    throw_type_error(
        ctx,
        &format!(
            "The \"{name}\" argument must be of type number. Received {}",
            argument_received_str(value)
        ),
        "ERR_INVALID_ARG_TYPE",
    )
}

/// The `Received …` clause for `ERR_INVALID_ARG_TYPE` messages, in Node's
/// shape (strings render as `type string ('x')`, booleans as
/// `type boolean (true)`, `null`/`undefined` bare).
fn argument_received_str(value: &Value<'_>) -> String {
    match value.type_of() {
        Type::Undefined => "undefined".to_string(),
        Type::Null => "null".to_string(),
        Type::Int | Type::Float => value.as_number().map_or_else(String::new, js_number_str),
        Type::String => {
            let text = value
                .as_string()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_default();
            format!("type string ('{text}')")
        }
        Type::Bool => format!(
            "type boolean ({})",
            if value.as_bool().unwrap_or(false) {
                "true"
            } else {
                "false"
            }
        ),
        other => other.as_str().to_string(),
    }
}

fn value_out_of_range(ctx: &Ctx<'_>, must_be: &str, received: &str) -> rquickjs::Error {
    throw_range_error(
        ctx,
        &format!(
            "The value of \"value\" is out of range. It must be {must_be}. Received {received}"
        ),
        "ERR_OUT_OF_RANGE",
    )
}

/// Node's `writeBig*64` range error: the bounds text is BigInt literal
/// notation and the received value keeps the `n` suffix.
fn big_value_out_of_range<'js>(
    ctx: &Ctx<'js>,
    signed: bool,
    value: &Value<'js>,
) -> rquickjs::Error {
    let bounds = if signed {
        ">= -(2n ** 63n) and < 2n ** 63n"
    } else {
        ">= 0n and < 2n ** 64n"
    };
    let received = Coerced::<String>::from_js(ctx, value.clone())
        .map(|text| format!("{}n", group_digits(&text.0)))
        .unwrap_or_else(|_| "a BigInt".to_string());
    throw_range_error(
        ctx,
        &format!(
            "The value of \"value\" is out of range. It must be {bounds}. Received {received}"
        ),
        "ERR_OUT_OF_RANGE",
    )
}

// ---------------------------------------------------------------------------
// Receiver byte access + shared validation (JS `check*` parity)
// ---------------------------------------------------------------------------

/// The receiver of a Buffer prototype method as its writable backing bytes.
/// Buffer instances are `Uint8Array` subclass instances, so the engine's
/// typed-array view applies (`Object::as_typed_array::<u8>` →
/// `TypedArray::as_raw`, which applies the view's `byteOffset`).
///
/// `mut_from_ref` is the point of this helper: the `&mut` aliases the
/// engine-owned backing store, not the handle — the runtime is
/// single-threaded, the engine is suspended inside the native call, and no
/// other reference to the store can exist for the slice's lifetime.
#[allow(unsafe_code, clippy::mut_from_ref)]
fn buffer_bytes<'x, 'js>(this: &'x Object<'js>) -> Option<&'x mut [u8]> {
    let typed = this.as_typed_array::<u8>()?;
    let raw = typed.as_raw()?;
    // SAFETY: `raw` is the live backing store of a still-attached typed
    // array; the engine is suspended inside this native call, so no JS runs
    // (and the store cannot be reallocated or detached) while the slice is
    // held.
    Some(unsafe { std::slice::from_raw_parts_mut(raw.as_ptr().cast::<u8>(), raw.len()) })
}

/// Node/V8 `checkBounds`/`boundsError` parity (v24): a missing/undefined
/// offset is 0; a non-number is `ERR_INVALID_ARG_TYPE`; a non-integer is the
/// "must be an integer" text; a buffer shorter than the access throws
/// `ERR_BUFFER_OUT_OF_BOUNDS`; any other offset past `len - size` carries the
/// bounded-range text — the last three are all `RangeError`s.
fn checked_offset<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    offset: Option<&Value<'js>>,
    size: usize,
) -> Result<usize> {
    let len = match this.as_typed_array::<u8>() {
        Some(typed) => typed.len(),
        None => return Err(Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE)),
    };
    let bound = len as i64 - size as i64;
    let Some(offset) = offset else {
        return Ok(0); // missing argument behaves like `undefined` → 0
    };
    if offset.is_undefined() {
        return Ok(0);
    }
    let Some(raw) = offset.as_number() else {
        return Err(invalid_arg_type(ctx, "offset", offset));
    };
    if !raw.is_finite() || raw.fract() != 0.0 {
        return Err(throw_range_error(
            ctx,
            &offset_integer_message(&js_number_str(raw)),
            "ERR_OUT_OF_RANGE",
        ));
    }
    // Node's `boundsError` checks the derived bound before the offset range:
    // a buffer shorter than the access is ERR_BUFFER_OUT_OF_BOUNDS.
    if bound < 0 {
        return Err(buffer_out_of_bounds_access(ctx));
    }
    if raw < 0.0 || raw > bound as f64 {
        return Err(offset_out_of_range(ctx, bound, &js_number_str(raw)));
    }
    Ok(raw as usize)
}

/// Node/V8 `checkNumericValue` parity (v24): the value is coerced with `+v`
/// (strings/booleans/NaN pass through to the range check) and the range
/// failure carries the `>= min and <= max` bounds text.
fn checked_int_value<'js>(
    ctx: &Ctx<'js>,
    value: Option<&Value<'js>>,
    min: f64,
    max: f64,
) -> Result<f64> {
    let v = match value {
        Some(v) if !v.is_undefined() => Coerced::<f64>::from_js(ctx, v.clone())?.0,
        _ => f64::NAN, // missing argument behaves like `undefined`
    };
    if v < min || v > max {
        return Err(value_out_of_range(
            ctx,
            &format!(">= {min} and <= {max}"),
            &js_number_str(v),
        ));
    }
    Ok(v)
}

/// JS `DataView.set*`-style numeric coercion for float writes (`Number(v)`).
fn coerce_number<'js>(ctx: &Ctx<'js>, value: Option<&Value<'js>>) -> Result<f64> {
    match value {
        Some(v) if !v.is_undefined() => Ok(Coerced::<f64>::from_js(ctx, v.clone())?.0),
        _ => Ok(f64::NAN),
    }
}

// ---------------------------------------------------------------------------
// Numeric ops
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn do_write_int<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    value: Option<&Value<'js>>,
    offset: Option<&Value<'js>>,
    min: f64,
    max: f64,
    size: usize,
    big_endian: bool,
) -> Result<i64> {
    // JS shim order: offset validity first, then the value range.
    let off = checked_offset(ctx, this, offset, size)?;
    let v = checked_int_value(ctx, value, min, max)? as i64;
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    let raw = v.to_le_bytes();
    let dst = &mut bytes[off..off + size];
    for (i, slot) in dst.iter_mut().enumerate() {
        *slot = if big_endian {
            raw[size - 1 - i]
        } else {
            raw[i]
        };
    }
    Ok(off as i64 + size as i64)
}

fn do_read_int<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    offset: Option<&Value<'js>>,
    signed: bool,
    size: usize,
    big_endian: bool,
) -> Result<i64> {
    let off = checked_offset(ctx, this, offset, size)?;
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    let slice = &bytes[off..off + size];
    let v = if big_endian {
        slice.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
    } else {
        slice
            .iter()
            .rev()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
    };
    Ok(if signed {
        match size {
            1 => (v as u8) as i8 as i64,
            2 => (v as u16) as i16 as i64,
            4 => (v as u32) as i32 as i64,
            _ => v as i64,
        }
    } else {
        v as i64
    })
}

fn do_write_float<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    value: Option<&Value<'js>>,
    offset: Option<&Value<'js>>,
    double: bool,
    big_endian: bool,
) -> Result<i64> {
    let size = if double { 8 } else { 4 };
    let off = checked_offset(ctx, this, offset, size)?;
    let v = coerce_number(ctx, value)?;
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    if double {
        let raw = (v as f64).to_bits().to_le_bytes();
        write_endian(bytes, off, &raw, big_endian);
    } else {
        let raw = (v as f32).to_le_bytes();
        write_endian(bytes, off, &raw, big_endian);
    }
    Ok(off as i64 + size as i64)
}

fn do_read_float<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    offset: Option<&Value<'js>>,
    double: bool,
    big_endian: bool,
) -> Result<f64> {
    let size = if double { 8 } else { 4 };
    let off = checked_offset(ctx, this, offset, size)?;
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    if double {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[off..off + 8]);
        Ok(if big_endian {
            f64::from_be_bytes(raw)
        } else {
            f64::from_le_bytes(raw)
        })
    } else {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[off..off + 4]);
        let value = if big_endian {
            f32::from_be_bytes(raw)
        } else {
            f32::from_le_bytes(raw)
        };
        Ok(f64::from(value))
    }
}

/// Copies `raw` (little-endian bytes of the value) into `bytes[off..]`,
/// reversing the order for big-endian targets.
fn write_endian(bytes: &mut [u8], off: usize, raw: &[u8], big_endian: bool) {
    let size = raw.len();
    let dst = &mut bytes[off..off + size];
    for (i, slot) in dst.iter_mut().enumerate() {
        *slot = if big_endian {
            raw[size - 1 - i]
        } else {
            raw[i]
        };
    }
}

/// JS `checkBigIntValue`-shaped write. The value must be a BigInt; its bits
/// are written two's-complement. QuickJS reduces any BigInt modulo 2^64
/// (`JS_ToBigInt64Free`), matching `DataView.setBigInt64`; a BigInt outside
/// the 64-bit range therefore wraps instead of throwing (Node would throw
/// `ERR_OUT_OF_RANGE`) — a documented edge for pathological inputs.
fn do_write_big<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    value: Option<&Value<'js>>,
    offset: Option<&Value<'js>>,
    signed: bool,
    big_endian: bool,
) -> Result<i64> {
    let off = checked_offset(ctx, this, offset, 8)?;
    // Node checks the BigInt against its 64-bit window (ERR_OUT_OF_RANGE).
    // rquickjs's `to_i64` wraps modulo 2^64, so the check runs on the decimal
    // text instead (BigInt toString is plain digits; i128 covers both windows
    // and rejects anything larger by parse overflow).
    let bits: u64 = match value {
        Some(v) if v.is_big_int() => {
            let parsed: Option<i128> = Coerced::<String>::from_js(ctx, v.clone())
                .ok()
                .and_then(|text| text.0.parse::<i128>().ok());
            match (signed, parsed) {
                (true, Some(bits)) if (-(1i128 << 63)..(1i128 << 63)).contains(&bits) => {
                    bits as i64 as u64
                }
                (false, Some(bits)) if (0..(1i128 << 64)).contains(&bits) => bits as u64,
                _ => return Err(big_value_out_of_range(ctx, signed, v)),
            }
        }
        // V8's raw TypeError for `BigInt` arithmetic with other types (no
        // `code`), which is what `DataView.setBig*` surfaces for
        // wrong/missing arguments.
        _ => {
            return Err(Exception::throw_type(
                ctx,
                "Cannot mix BigInt and other types, use explicit conversions",
            ))
        }
    };
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    let raw = if signed {
        (bits as i64).to_le_bytes()
    } else {
        bits.to_le_bytes()
    };
    write_endian(bytes, off, &raw, big_endian);
    Ok(off as i64 + 8)
}

fn do_read_big<'js>(
    ctx: &Ctx<'js>,
    this: &Object<'js>,
    offset: Option<&Value<'js>>,
    signed: bool,
    big_endian: bool,
) -> Result<Value<'js>> {
    let off = checked_offset(ctx, this, offset, 8)?;
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[off..off + 8]);
    if big_endian {
        // The constructors below read little-endian bit patterns.
        raw.reverse();
    }
    let big = if signed {
        BigInt::from_i64(ctx.clone(), i64::from_le_bytes(raw))?
    } else {
        BigInt::from_u64(ctx.clone(), u64::from_le_bytes(raw))?
    };
    Ok(big.into_value())
}

fn do_swap<'js>(ctx: &Ctx<'js>, this: &Object<'js>, size: usize) -> Result<Value<'js>> {
    let bytes =
        buffer_bytes(this).ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    if bytes.len() % size != 0 {
        let bits = size * 8;
        return Err(throw_range_error(
            ctx,
            &format!("Buffer size must be a multiple of {bits}-bits"),
            "ERR_INVALID_BUFFER_SIZE",
        ));
    }
    for chunk in bytes.chunks_exact_mut(size) {
        chunk.reverse();
    }
    Ok(this.clone().into_value())
}

// ---------------------------------------------------------------------------
// Codec dispatcher (`__pirBufferCodec`)
// ---------------------------------------------------------------------------

/// The encode/decode dispatcher behind the JS codec state machines. Ops that
/// take a string return `undefined` when the string cannot cross the host
/// bridge (lone surrogates); the JS state machine is the fallback. Ops that
/// take bytes always succeed (fatal-mode decode failures throw TypeError).
///
/// The trailing arguments carry per-op payloads the earlier ops do not need:
/// `arg2` is the `node:string_decoder` receiver (or a search needle), `start`
/// the resolved search offset; `flag_a`/`flag_b` are the search direction and
/// its 16-bit alignment for `indexOfBytes`.
fn buffer_codec_entry<'js>(
    ctx: Ctx<'js>,
    op: String,
    arg: Opt<Value<'js>>,
    flag_a: Opt<bool>,
    flag_b: Opt<bool>,
    arg2: Opt<Value<'js>>,
    start: Opt<f64>,
) -> Result<Value<'js>> {
    let undefined = |ctx: &Ctx<'js>| Ok(Value::new_undefined(ctx.clone()));
    let arg = arg.0.as_ref();
    match op.as_str() {
        // -- string → bytes (undefined ⇒ JS fallback) ------------------------
        "utf8Bytes" => {
            let Some(text) = marshal_string(arg) else {
                return undefined(&ctx);
            };
            typed_array_value(ctx, text.into_bytes())
        }
        "latin1Bytes" => {
            let Some(text) = marshal_string(arg) else {
                return undefined(&ctx);
            };
            // One byte per UTF-16 unit, and a unit never outgrows the UTF-8
            // bytes of its char: `len()` bounds the output exactly, so the
            // `collect`'s `size_hint`-derived growth is replaced by one
            // allocation.
            let mut bytes = Vec::with_capacity(text.len());
            bytes.extend(text.encode_utf16().map(|unit| (unit & 0xff) as u8));
            typed_array_value(ctx, bytes)
        }
        "utf16leBytes" => {
            let Some(text) = marshal_string(arg) else {
                return undefined(&ctx);
            };
            // Two bytes per unit under the same bound, hence `len() * 2` — an
            // exact bound the flattening iterator cannot report: its hint
            // assumes 3-byte sequences, so it proves only two thirds of the
            // input (a third of this output).
            let mut bytes = Vec::with_capacity(text.len() * 2);
            bytes.extend(
                text.encode_utf16()
                    .flat_map(|unit| [unit as u8, (unit >> 8) as u8]),
            );
            typed_array_value(ctx, bytes)
        }
        "hexBytes" => {
            let Some(text) = marshal_string(arg) else {
                return undefined(&ctx);
            };
            typed_array_value(ctx, hex_decode(&text))
        }
        "base64Bytes" => {
            let Some(text) = marshal_string(arg) else {
                return undefined(&ctx);
            };
            typed_array_value(ctx, base64_decode(&text))
        }

        // -- bytes → string ---------------------------------------------------
        "utf8Decode" => {
            let bytes = codec_bytes(arg, &ctx)?;
            // SAFETY: no JS runs while the slice is held (see `buffer_bytes`).
            let text = typed_read(bytes).map(utf8_decode).unwrap_or_default();
            text.into_js(&ctx)
        }
        "latin1Decode" => {
            let bytes = codec_bytes(arg, &ctx)?;
            // SAFETY: as above.
            let raw = typed_read(bytes).unwrap_or(&[]);
            // One allocation sized for the input: a `char`-wise `collect` has
            // no byte-length hint and would grow the string by repeated
            // reallocation (a latin1 byte can be a 2-byte char, so this is a
            // lower bound — the common ASCII input fits exactly).
            let mut text = String::with_capacity(raw.len());
            text.extend(raw.iter().map(|&b| b as char));
            text.into_js(&ctx)
        }
        "hexEncode" => {
            let bytes = codec_bytes(arg, &ctx)?;
            // SAFETY: as above.
            let text = typed_read(bytes).map(hex_encode).unwrap_or_default();
            text.into_js(&ctx)
        }
        "base64Encode" => {
            let bytes = codec_bytes(arg, &ctx)?;
            // SAFETY: as above.
            let text = typed_read(bytes).map(base64_encode).unwrap_or_default();
            text.into_js(&ctx)
        }

        // -- streaming chunk decodes (return {text, consumed}) ----------------
        "utf8DecodeChunk" => {
            let bytes = codec_bytes(arg, &ctx)?;
            let is_final = flag_a.0.unwrap_or(true);
            let fatal = flag_b.0.unwrap_or(false);
            // SAFETY: as above.
            let raw = typed_read(bytes).unwrap_or(&[]);
            let (text, consumed) = utf8_decode_chunk(&ctx, raw, is_final, fatal)?;
            chunk_result(&ctx, &text, consumed)
        }
        "utf16leDecodeChunk" => {
            let bytes = codec_bytes(arg, &ctx)?;
            // SAFETY: as above.
            let raw = typed_read(bytes).unwrap_or(&[]);
            // Unpaired surrogates cannot cross the host bridge → JS fallback.
            let Some((text, consumed)) = utf16le_decode_chunk(raw) else {
                return undefined(&ctx);
            };
            chunk_result(&ctx, &text, consumed)
        }
        // -- node:string_decoder streaming chunk (state carried in/out) --------
        "stringDecodeChunk" => {
            let Some((encoding, mut state)) = sd_read_state(arg2.0.as_ref()) else {
                return undefined(&ctx);
            };
            let Some(view) = codec_view(arg) else {
                return undefined(&ctx);
            };
            // SAFETY: no JS runs while the slice is held (see `buffer_bytes`).
            let Some(data) = typed_read(view) else {
                return undefined(&ctx);
            };
            let Some(text) = sd_write(encoding, &mut state, data) else {
                return undefined(&ctx);
            };
            sd_chunk_result(&ctx, &text, state)
        }
        "stringDecodeFlush" => {
            let Some((encoding, state)) = sd_read_state(arg2.0.as_ref()) else {
                return undefined(&ctx);
            };
            match sd_flush(encoding, &state.pending) {
                Some(text) => text.into_js(&ctx),
                None => undefined(&ctx),
            }
        }

        // -- Buffer search scan (offset/needle resolution stays in JS) --------
        "indexOfBytes" => {
            let Some(view) = codec_view(arg) else {
                return undefined(&ctx);
            };
            // SAFETY: no JS runs while the slices are held (see `buffer_bytes`).
            let Some(haystack) = typed_read(view) else {
                return undefined(&ctx);
            };
            let Some(needle) = codec_needle(arg2.0.as_ref()) else {
                return undefined(&ctx);
            };
            let Some(start) = start.0 else {
                return undefined(&ctx);
            };
            if !start.is_finite() || start.fract() != 0.0 || start < -1.0 {
                return undefined(&ctx);
            }
            let backwards = flag_a.0.unwrap_or(false);
            let align = if flag_b.0.unwrap_or(false) { 2 } else { 1 };
            let found = index_of_bytes(haystack, &needle, start as i64, backwards, align);
            Ok(Value::new_float(ctx, found as f64))
        }

        // -- variadic (1..6 byte) LE/BE integer kernels ------------------------
        "variadicWrite" => {
            let Some(view) = codec_view(arg) else {
                return undefined(&ctx);
            };
            let Some(value) = variadic_value(arg2.0.as_ref()) else {
                return undefined(&ctx);
            };
            let Some((offset, byte_length)) = variadic_unpack(start.0) else {
                return undefined(&ctx);
            };
            // SAFETY: no JS runs while the slice is held (see `buffer_bytes`).
            let Some(buffer) = typed_bytes(view) else {
                return undefined(&ctx);
            };
            if offset + byte_length > buffer.len() {
                return undefined(&ctx);
            }
            write_variadic_bytes(
                buffer,
                offset,
                byte_length,
                value,
                flag_a.0.unwrap_or(false),
            );
            Ok(Value::new_float(ctx, (offset + byte_length) as f64))
        }
        "variadicRead" => {
            let Some(view) = codec_view(arg) else {
                return undefined(&ctx);
            };
            let Some((offset, byte_length)) = variadic_unpack(start.0) else {
                return undefined(&ctx);
            };
            // SAFETY: no JS runs while the slice is held (see `buffer_bytes`).
            let Some(buffer) = typed_read(view) else {
                return undefined(&ctx);
            };
            if offset + byte_length > buffer.len() {
                return undefined(&ctx);
            }
            let value = read_variadic_bytes(
                buffer,
                offset,
                byte_length,
                flag_b.0.unwrap_or(false),
                flag_a.0.unwrap_or(false),
            );
            Ok(Value::new_float(ctx, value))
        }

        // -- util.inspect string escaper (lone surrogates stay in JS) ----------
        "strEscape" => {
            let Some(text) = marshal_string(arg) else {
                return undefined(&ctx);
            };
            str_escape(&text).into_js(&ctx)
        }

        other => Err(Exception::throw_type(
            &ctx,
            &format!("unknown __pirBufferCodec op: {other}"),
        )),
    }
}

fn typed_array_value<'js>(ctx: Ctx<'js>, bytes: Vec<u8>) -> Result<Value<'js>> {
    Ok(TypedArray::<u8>::new_copy(ctx, bytes)?.into_value())
}

/// Interprets a dispatcher argument as a `Uint8Array` (Buffer instances
/// included). Anything else is a JS-side wiring bug, hence a TypeError.
fn codec_bytes<'a, 'js>(
    arg: Option<&'a Value<'js>>,
    ctx: &Ctx<'js>,
) -> Result<&'a TypedArray<'js, u8>> {
    let value = arg.ok_or_else(|| Exception::throw_type(ctx, RECEIVER_TYPE_MESSAGE))?;
    value
        .as_object()
        .and_then(|object| object.as_typed_array::<u8>())
        .ok_or_else(|| Exception::throw_type(ctx, "expected a Uint8Array codec argument"))
}

/// [`codec_bytes`] as an `Option`: the `node:string_decoder` ops treat a view
/// that is not a plain `Uint8Array` (DataView, another element type, a
/// detached buffer) as "not mine" and let the JS shim's `pirViewToBytes`
/// handle it.
fn codec_view<'a, 'js>(arg: Option<&'a Value<'js>>) -> Option<&'a TypedArray<'js, u8>> {
    arg?.as_object()?.as_typed_array::<u8>()
}

fn chunk_result<'js>(ctx: &Ctx<'js>, text: &str, consumed: usize) -> Result<Value<'js>> {
    let result = Object::new(ctx.clone())?;
    result.set("text", text)?;
    result.set("consumed", consumed as i32)?;
    Ok(result.into_value())
}

/// JS string → Rust string; `None` when the value is not a string or cannot
/// cross the host bridge (lone surrogates) — the JS state machine fallback
/// handles both.
fn marshal_string(arg: Option<&Value<'_>>) -> Option<String> {
    let value = arg?;
    if !value.is_string() {
        return None;
    }
    value.as_string().and_then(|s| s.to_string().ok())
}

// ---------------------------------------------------------------------------
// Codec primitives (byte↔string; semantics mirror the JS state machines)
// ---------------------------------------------------------------------------

/// WHATWG-style UTF-8 decode with replacement semantics, mirroring the JS
/// `utf8Decode`: invalid sequences become U+FFFD and a truncated trailing
/// sequence is replaced once for its maximal subpart.
fn utf8_decode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        if b0 < 0x80 {
            out.push(b0 as char);
            i += 1;
            continue;
        }
        let need = if (0xc2..=0xdf).contains(&b0) {
            1
        } else if (0xe0..=0xef).contains(&b0) {
            2
        } else if (0xf0..=0xf4).contains(&b0) {
            3
        } else {
            out.push('\u{FFFD}');
            i += 1;
            continue;
        };
        if i + need >= bytes.len() {
            // Truncated trailing sequence: one replacement for the maximal subpart.
            out.push('\u{FFFD}');
            i += 1;
            while i < bytes.len() && (0x80..=0xbf).contains(&bytes[i]) {
                i += 1;
            }
            continue;
        }
        let mut valid = true;
        for continuation in &bytes[i + 1..=i + need] {
            if !(0x80..=0xbf).contains(continuation) {
                valid = false;
                break;
            }
        }
        if !valid {
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        let cp: u32 = match need {
            1 => (u32::from(b0 & 0x1f) << 6) | u32::from(bytes[i + 1] & 0x3f),
            2 => {
                (u32::from(b0 & 0x0f) << 12)
                    | (u32::from(bytes[i + 1] & 0x3f) << 6)
                    | u32::from(bytes[i + 2] & 0x3f)
            }
            _ => {
                (u32::from(b0 & 0x07) << 18)
                    | (u32::from(bytes[i + 1] & 0x3f) << 12)
                    | (u32::from(bytes[i + 2] & 0x3f) << 6)
                    | u32::from(bytes[i + 3] & 0x3f)
            }
        };
        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
        i += need + 1;
    }
    out
}

/// Streaming variant for TextDecoder: returns `(text, consumed)`. An
/// incomplete trailing sequence stays unconsumed unless `is_final`, in which
/// case it becomes U+FFFD (or a fatal TypeError). Invalid sequences are
/// U+FFFD — or a fatal TypeError. Mirrors the JS TextDecoder semantics.
fn utf8_decode_chunk(
    ctx: &Ctx<'_>,
    bytes: &[u8],
    is_final: bool,
    fatal: bool,
) -> Result<(String, usize)> {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        if b0 < 0x80 {
            out.push(b0 as char);
            i += 1;
            continue;
        }
        let need = if (0xc2..=0xdf).contains(&b0) {
            1
        } else if (0xe0..=0xef).contains(&b0) {
            2
        } else if (0xf0..=0xf4).contains(&b0) {
            3
        } else {
            if fatal {
                return Err(Exception::throw_type(ctx, FATAL_DECODE_MESSAGE));
            }
            out.push('\u{FFFD}');
            i += 1;
            continue;
        };
        if bytes.len() - i - 1 < need {
            if is_final {
                if fatal {
                    return Err(Exception::throw_type(ctx, FATAL_DECODE_MESSAGE));
                }
                out.push('\u{FFFD}');
                return Ok((out, bytes.len()));
            }
            // Incomplete trailing sequence: leave it for the next chunk.
            return Ok((out, i));
        }
        let mut valid = true;
        for continuation in &bytes[i + 1..=i + need] {
            if !(0x80..=0xbf).contains(continuation) {
                valid = false;
                break;
            }
        }
        if !valid {
            if fatal {
                return Err(Exception::throw_type(ctx, FATAL_DECODE_MESSAGE));
            }
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        let cp: u32 = match need {
            1 => (u32::from(b0 & 0x1f) << 6) | u32::from(bytes[i + 1] & 0x3f),
            2 => {
                (u32::from(b0 & 0x0f) << 12)
                    | (u32::from(bytes[i + 1] & 0x3f) << 6)
                    | u32::from(bytes[i + 2] & 0x3f)
            }
            _ => {
                (u32::from(b0 & 0x07) << 18)
                    | (u32::from(bytes[i + 1] & 0x3f) << 12)
                    | (u32::from(bytes[i + 2] & 0x3f) << 6)
                    | u32::from(bytes[i + 3] & 0x3f)
            }
        };
        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
        i += need + 1;
    }
    Ok((out, bytes.len()))
}

/// UTF-16LE decode over whole bytes; the odd trailing byte is not consumed
/// (streaming carries it, the final flush drops it — JS parity). Returns
/// `None` when the data contains unpaired surrogates (cannot cross the host
/// bridge → JS fallback).
fn utf16le_decode_chunk(bytes: &[u8]) -> Option<(String, usize)> {
    let paired_len = bytes.len() - (bytes.len() % 2);
    // Decoded straight off the input pairs: materializing the code units in a
    // `Vec<u16>` first would add a second buffer the length of the chunk (plus
    // a write and a read pass over it) for nothing.
    let unit_at = |at: usize| u16::from(bytes[at]) | (u16::from(bytes[at + 1]) << 8);
    let mut out = String::with_capacity(paired_len / 2);
    let mut at = 0;
    while at < paired_len {
        let unit = unit_at(at);
        if (0xd800..0xdc00).contains(&unit) {
            // A high surrogate needs its low partner: without one the string
            // cannot cross the host bridge and the JS tier takes the chunk.
            if at + 4 > paired_len {
                return None;
            }
            let low = unit_at(at + 2);
            if !(0xdc00..0xe000).contains(&low) {
                return None;
            }
            let cp = 0x1_0000 + ((u32::from(unit - 0xd800) << 10) | u32::from(low - 0xdc00));
            out.push(char::from_u32(cp)?);
            at += 4;
        } else if (0xdc00..0xe000).contains(&unit) {
            return None; // lone low surrogate
        } else {
            out.push(char::from_u32(u32::from(unit))?);
            at += 2;
        }
    }
    Some((out, paired_len))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Hex decode mirroring the JS `hexBytes`: stops at the first invalid pair
/// and ignores a trailing single character.
fn hex_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        let (Some(hi), Some(lo)) = (hex_nibble(bytes[i]), hex_nibble(bytes[i + 1])) else {
            break;
        };
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    out
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        // Node's decoder table accepts the URL-safe alphabet in both modes
        // (`Buffer.from('__--_--_--__', 'base64')` decodes like base64url).
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Base64 decode mirroring the JS `base64Bytes`: `=` ends the payload,
/// non-alphabet characters are skipped, bits flush on byte boundaries.
fn base64_decode(text: &str) -> Vec<u8> {
    // Every four input characters carry at most three output bytes (skipped
    // characters and `=` only lower that), so this is a hard upper bound and
    // the buffer never has to grow — same sizing idiom as `base64_encode`.
    let mut out = Vec::with_capacity(text.len().div_ceil(4) * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in text.as_bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = base64_value(byte) else {
            continue;
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

/// Base64 encode with standard `=` padding (JS `base64Encode` parity).
fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied();
        let b2 = bytes.get(i + 2).copied();
        out.push(CHARS[(b0 >> 2) as usize] as char);
        out.push(CHARS[(((b0 & 3) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => CHARS[(((b1 & 15) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => CHARS[(b2 & 63) as usize] as char,
            None => '=',
        });
        i += 3;
    }
    out
}

// ---------------------------------------------------------------------------
// node:string_decoder streaming ops (`stringDecodeChunk` / `stringDecodeFlush`)
// ---------------------------------------------------------------------------
//
// Byte-for-byte port of `extensions/api/js/string_decoder_shim.js.txt` — the
// JS state machine stays in the file as the fallback, so the two must agree on
// every reachable state. Node's C++ `StringDecoder` semantics are reproduced
// exactly (the shim was validated against node v24 by differential probes):
// UTF-8 holds the trailing incomplete character found by `DecodeData`'s
// walk-back, a held character is refilled byte by byte (continuation bytes
// only for UTF-8) and decoded as soon as it completes, utf16le holds a lone
// trailing byte or a split surrogate pair, and base64/base64url hold
// `nread % 3` bytes.

/// A pending byte count or `need` above these bounds cannot come out of the JS
/// state machine (pending is at most one 4-byte character, `need` at most 3);
/// a hand-tampered receiver then keeps the JS tier in charge.
const SD_MAX_PENDING: usize = 4;
const SD_MAX_NEED: usize = 4;

const SD_BASE64_CHARS: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const SD_BASE64URL_CHARS: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The normalized encodings `normalizeEncoding` can produce; anything else
/// (an object without a normalized `encoding`) keeps the JS state machine.
#[derive(Clone, Copy)]
enum SdEncoding {
    Utf8,
    Utf16le,
    Base64,
    Base64Url,
    Hex,
    Ascii,
    Latin1,
}

impl SdEncoding {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "utf8" => Self::Utf8,
            "utf16le" => Self::Utf16le,
            "base64" => Self::Base64,
            "base64url" => Self::Base64Url,
            "hex" => Self::Hex,
            "ascii" => Self::Ascii,
            "latin1" => Self::Latin1,
            _ => return None,
        })
    }
}

/// The decoder state the JS shim keeps in `_pirPending` (held bytes),
/// `_pirNeed` (bytes still missing) and `_pirWindow` (the 4-byte `lastChar`
/// window, whose positions past the held character keep stale bytes).
struct SdState {
    pending: Vec<u8>,
    need: usize,
    window: [u8; 4],
}

impl SdState {
    /// JS `pirWriteWindow`: byte `i` lands at `offset + i`, nothing is written
    /// past the 4-byte window.
    fn write_window(&mut self, bytes: &[u8], offset: usize) {
        for (i, &byte) in bytes.iter().enumerate() {
            if offset + i < 4 {
                self.window[offset + i] = byte;
            }
        }
    }

    /// JS `pirSetPending`.
    fn set_pending(&mut self, bytes: &[u8], need: usize) {
        self.pending = bytes.to_vec();
        self.need = need;
        self.write_window(bytes, 0);
    }

    /// JS `pirClearPending` (the window keeps its stale bytes).
    fn clear(&mut self) {
        self.pending.clear();
        self.need = 0;
    }
}

/// A JS array of integers as bytes; `None` when it is longer than `max_len` or
/// holds anything but an integer 0..=255 — a state the JS shim cannot produce.
fn sd_byte_array(array: &Array<'_>, max_len: usize) -> Option<Vec<u8>> {
    let len = array.len();
    if len > max_len {
        return None;
    }
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        let number = array.get::<Value>(index).ok()?.as_number()?;
        if number.fract() != 0.0 || !(0.0..=255.0).contains(&number) {
            return None;
        }
        out.push(number as u8);
    }
    Some(out)
}

/// The receiver of a `stringDecode*` op as `(encoding, state)`. `None` for
/// anything the JS tier should keep: a non-object receiver, a missing or
/// non-normalized `encoding`, or a state triple the shim could not produce.
fn sd_read_state(value: Option<&Value<'_>>) -> Option<(SdEncoding, SdState)> {
    let object = value?.as_object()?;
    let encoding = SdEncoding::from_name(object.get::<_, String>("encoding").ok()?.as_str())?;
    let pending_obj = object.get::<_, Object>("_pirPending").ok()?;
    let pending = sd_byte_array(&pending_obj.into_array()?, SD_MAX_PENDING)?;
    let need = object.get::<_, Value>("_pirNeed").ok()?.as_number()?;
    if need.fract() != 0.0 || !(0.0..=SD_MAX_NEED as f64).contains(&need) {
        return None;
    }
    let window_obj = object.get::<_, Object>("_pirWindow").ok()?;
    let window: [u8; 4] = sd_byte_array(&window_obj.into_array()?, 4)?
        .try_into()
        .ok()?;
    Some((
        encoding,
        SdState {
            pending,
            need: need as usize,
            window,
        },
    ))
}

/// The `{text, pending, need, window}` shape the JS wrapper applies to the
/// decoder's three state fields (`Vec<u8>` crosses as a JS array of numbers,
/// so the fallback state machine sees the same plain arrays it builds). The
/// state is consumed — every caller drops it right after — so the held bytes
/// move into the result and the window is marshalled straight from its array,
/// with neither the clone nor the scratch `Vec` of the borrowed version.
fn sd_chunk_result<'js>(ctx: &Ctx<'js>, text: &str, state: SdState) -> Result<Value<'js>> {
    let result = Object::new(ctx.clone())?;
    result.set("text", text)?;
    result.set("pending", state.pending)?;
    result.set("need", state.need as i32)?;
    result.set("window", &state.window[..])?;
    Ok(result.into_value())
}

/// JS `pirWriteChunk`. `None` when the chunk's text cannot be a Rust string
/// (a utf16le lone surrogate) — the JS state machine then runs this chunk.
fn sd_write(encoding: SdEncoding, state: &mut SdState, data: &[u8]) -> Option<String> {
    match encoding {
        SdEncoding::Utf8 => sd_utf8_write(state, data),
        SdEncoding::Utf16le => sd_utf16_write(state, data),
        SdEncoding::Base64 => sd_base64_write(state, data, false),
        SdEncoding::Base64Url => sd_base64_write(state, data, true),
        // Stateless encodings: the decoder never holds anything, and the
        // 4-byte window passes through untouched.
        SdEncoding::Hex => Some(hex_encode(data)),
        SdEncoding::Ascii => Some(byte_map_string(data, |byte| (byte & 0x7f) as char)),
        SdEncoding::Latin1 => Some(byte_map_string(data, |byte| byte as char)),
    }
}

/// One `char` per input byte (the stateless `SdEncoding` arms). Sized once for
/// the chunk, since a per-byte push has no byte-length hint to reserve from.
fn byte_map_string(data: &[u8], map: impl Fn(u8) -> char) -> String {
    let mut out = String::with_capacity(data.len());
    out.extend(data.iter().map(|&byte| map(byte)));
    out
}

/// JS `pirFlush`: the held character decoded at a write boundary (`end()`), a
/// 1- or 2-byte tail padded for base64, never for base64url.
fn sd_flush(encoding: SdEncoding, pending: &[u8]) -> Option<String> {
    if pending.is_empty() {
        return Some(String::new());
    }
    match encoding {
        SdEncoding::Utf8 => Some(sd_utf8_decode(pending)),
        SdEncoding::Utf16le => sd_utf16_decode(pending),
        SdEncoding::Base64 => Some(sd_base64_encode(pending, SD_BASE64_CHARS, true)),
        SdEncoding::Base64Url => Some(sd_base64_encode(pending, SD_BASE64URL_CHARS, false)),
        _ => Some(String::new()),
    }
}

/// The write path's accumulator: `head` is the text of the refilled held
/// character (`None` when there was nothing to refill), `tail` the chunk's own
/// text. Only the held-character path has anything to concatenate — the
/// stateless path (the common one) returns the decoded chunk as it is, so no
/// accumulator is allocated and the text is not copied a second time.
fn sd_join(head: Option<String>, tail: String) -> String {
    match head {
        Some(mut head) => {
            head.push_str(&tail);
            head
        }
        None => tail,
    }
}

/// JS `pirUtf8Write`: refill the held character, then hold the new trailing
/// incomplete one (or decode everything).
fn sd_utf8_write(state: &mut SdState, data: &[u8]) -> Option<String> {
    let mut head = None;
    let mut rest = data;
    if state.need > 0 && !state.pending.is_empty() {
        let (text, consumed, held_all) = sd_fill_pending(SdEncoding::Utf8, state, data, true)?;
        if held_all {
            return Some(text);
        }
        head = Some(text);
        rest = &data[consumed..];
    }
    if rest.is_empty() {
        return Some(head.unwrap_or_default());
    }
    let hold = sd_utf8_hold_count(rest);
    let tail = if hold > 0 {
        let need = sd_utf8_missing_for(rest);
        state.set_pending(&rest[rest.len() - hold..], need);
        sd_utf8_decode(&rest[..rest.len() - hold])
    } else {
        state.clear();
        sd_utf8_decode(rest)
    };
    Some(sd_join(head, tail))
}

/// JS `pirUtf16Write` (UCS-2: a lone trailing byte and a split surrogate pair
/// are held verbatim).
fn sd_utf16_write(state: &mut SdState, data: &[u8]) -> Option<String> {
    let mut head = None;
    let mut rest = data;
    if state.need > 0 && !state.pending.is_empty() {
        let (text, consumed, held_all) = sd_fill_pending(SdEncoding::Utf16le, state, data, false)?;
        if held_all {
            return Some(text);
        }
        head = Some(text);
        rest = &data[consumed..];
    }
    if rest.is_empty() {
        return Some(head.unwrap_or_default());
    }
    let (hold, missing) = if rest.len() % 2 == 1 {
        (1, 1)
    } else if (rest[rest.len() - 1] & 0xfc) == 0xd8 {
        (2, 2)
    } else {
        (0, 0)
    };
    let tail = if hold > 0 {
        state.set_pending(&rest[rest.len() - hold..], missing);
        sd_utf16_decode(&rest[..rest.len() - hold])?
    } else {
        state.clear();
        sd_utf16_decode(rest)?
    };
    Some(sd_join(head, tail))
}

/// JS `pirBase64Write`: only whole 3-byte groups are emitted, the `nread % 3`
/// tail is held for the next chunk (or the flush).
fn sd_base64_write(state: &mut SdState, data: &[u8], url: bool) -> Option<String> {
    let mut head = None;
    let mut rest = data;
    if state.need > 0 && !state.pending.is_empty() {
        let encoding = if url {
            SdEncoding::Base64Url
        } else {
            SdEncoding::Base64
        };
        let (text, consumed, held_all) = sd_fill_pending(encoding, state, data, false)?;
        if held_all {
            return Some(text);
        }
        head = Some(text);
        rest = &data[consumed..];
    }
    let tail_len = rest.len() % 3;
    let (body, held) = rest.split_at(rest.len() - tail_len);
    if tail_len > 0 {
        state.set_pending(held, 3 - tail_len);
    } else {
        state.clear();
    }
    Some(sd_join(
        head,
        sd_base64_encode(
            body,
            if url {
                SD_BASE64URL_CHARS
            } else {
                SD_BASE64_CHARS
            },
            false,
        ),
    ))
}

/// JS `pirFillPending`: Node's `DecodeData` byte-by-byte refill of the held
/// character. `continuation_only` is the UTF-8 rule (0x80-0xBF only); utf16le
/// and base64 accept any byte. Returns `(text, consumed, held_all)`, where
/// `consumed` is the chunk prefix that went into the held character and
/// `held_all` reports that the chunk was swallowed whole (the character is
/// still incomplete), so the caller has no remainder to process.
fn sd_fill_pending(
    encoding: SdEncoding,
    state: &mut SdState,
    data: &[u8],
    continuation_only: bool,
) -> Option<(String, usize, bool)> {
    let mut accepted = 0;
    while accepted < data.len() && accepted < state.need {
        if continuation_only && (data[accepted] & 0xc0) != 0x80 {
            break;
        }
        accepted += 1;
    }
    if accepted > 0 {
        let offset = state.pending.len();
        state.write_window(&data[..accepted], offset);
    }
    let mut held = state.pending.clone();
    held.extend_from_slice(&data[..accepted]);
    let missing = state.need - accepted;
    if missing == 0 {
        // The held character is complete.
        let text = sd_decode_held(encoding, &held)?;
        state.clear();
        return Some((text, accepted, false));
    }
    if accepted == data.len() {
        // Still incomplete: every byte of the chunk was a continuation.
        state.pending = held;
        state.need = missing;
        return Some((String::new(), accepted, true));
    }
    // Broken by a non-continuation byte: flush the maximal subpart and
    // reprocess the offending byte as the start of the next character.
    let text = sd_decode_held(encoding, &held)?;
    state.clear();
    Some((text, accepted, false))
}

/// JS `pirDecodeHeld`: the decode applied to a completed held character.
fn sd_decode_held(encoding: SdEncoding, bytes: &[u8]) -> Option<String> {
    match encoding {
        SdEncoding::Utf16le => sd_utf16_decode(bytes),
        SdEncoding::Base64 => Some(sd_base64_encode(bytes, SD_BASE64_CHARS, false)),
        SdEncoding::Base64Url => Some(sd_base64_encode(bytes, SD_BASE64URL_CHARS, false)),
        _ => Some(sd_utf8_decode(bytes)),
    }
}

/// V8's `StringBytes::Encode` maximal-subpart UTF-8 decode, i.e. the shim's
/// `pirUtf8Decode` — deliberately *not* [`utf8_decode`] above: the two differ
/// on a sequence broken by a non-continuation byte (`E2 82 41` is one U+FFFD
/// plus "A" here, two U+FFFD there).
fn sd_utf8_decode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let first = bytes[i];
        if first < 0x80 {
            out.push(first as char);
            i += 1;
            continue;
        }
        let (needed, mut code_point, mut lower, mut upper): (usize, u32, u8, u8) =
            if (0xc2..=0xdf).contains(&first) {
                (1, u32::from(first & 0x1f), 0x80, 0xbf)
            } else if (0xe0..=0xef).contains(&first) {
                let lower = if first == 0xe0 { 0xa0 } else { 0x80 };
                let upper = if first == 0xed { 0x9f } else { 0xbf };
                (2, u32::from(first & 0x0f), lower, upper)
            } else if (0xf0..=0xf4).contains(&first) {
                let lower = if first == 0xf0 { 0x90 } else { 0x80 };
                let upper = if first == 0xf4 { 0x8f } else { 0xbf };
                (3, u32::from(first & 0x07), lower, upper)
            } else {
                // 0x80-0xC1 and 0xF5-0xFF can never start a sequence.
                out.push('\u{fffd}');
                i += 1;
                continue;
            };
        let mut seen = 0;
        let mut ok = true;
        while seen < needed {
            // The JS `-1` sentinel: running past the end is never a valid
            // continuation.
            let Some(&next) = bytes.get(i + 1 + seen) else {
                ok = false;
                break;
            };
            if next < lower || next > upper {
                ok = false;
                break;
            }
            code_point = (code_point << 6) | u32::from(next & 0x3f);
            seen += 1;
            lower = 0x80;
            upper = 0xbf;
        }
        if ok {
            out.push(char::from_u32(code_point).unwrap_or('\u{fffd}'));
            i += needed + 1;
        } else {
            // One replacement char for the valid prefix and rescan the byte
            // that broke the sequence.
            out.push('\u{fffd}');
            i += seen + 1;
        }
    }
    out
}

/// JS `pirUtf8HoldCount`: how many trailing bytes form an incomplete UTF-8
/// character, via Node's `DecodeData` walk-back over the trailing continuation
/// run (0 when the tail is complete or undecidable).
fn sd_utf8_hold_count(bytes: &[u8]) -> usize {
    let length = bytes.len();
    if length == 0 {
        return 0;
    }
    let last = bytes[length - 1];
    if last < 0x80 {
        return 0; // an ASCII tail can never be incomplete
    }
    let start = if (last & 0xc0) == 0x80 {
        let mut start = length - 1;
        while start > 0 && (bytes[start - 1] & 0xc0) == 0x80 {
            start -= 1;
        }
        if start == 0 {
            return 0; // the run reaches the start: no lead byte
        }
        start - 1
    } else {
        length - 1
    };
    let lead = bytes[start];
    let expected = if (lead & 0xe0) == 0xc0 {
        2
    } else if (lead & 0xf0) == 0xe0 {
        3
    } else if (lead & 0xf8) == 0xf0 {
        4
    } else {
        return 0; // not a lead byte: the tail is never carried over
    };
    let held = length - start;
    if held < expected {
        held
    } else {
        0
    }
}

/// JS `pirUtf8NeedFor`; only called with a non-zero hold count.
fn sd_utf8_missing_for(bytes: &[u8]) -> usize {
    let hold = sd_utf8_hold_count(bytes);
    if hold == 0 {
        return 0;
    }
    let lead = bytes[bytes.len() - hold];
    if (lead & 0xe0) == 0xc0 {
        2 - hold
    } else if (lead & 0xf0) == 0xe0 {
        3 - hold
    } else {
        4 - hold
    }
}

/// JS `pirUtf16Decode`: little-endian units, one `String.fromCharCode` each —
/// an unpaired surrogate is a legal JS string but not a Rust `String`, so
/// `None` hands the chunk back to the JS state machine.
fn sd_utf16_decode(bytes: &[u8]) -> Option<String> {
    let length = bytes.len() - bytes.len() % 2;
    let mut out = String::with_capacity(length / 2);
    let mut i = 0;
    while i < length {
        let unit = u16::from(bytes[i]) | (u16::from(bytes[i + 1]) << 8);
        i += 2;
        if (0xd800..0xdc00).contains(&unit) {
            if i + 1 >= length {
                return None; // lone high surrogate
            }
            let low = u16::from(bytes[i]) | (u16::from(bytes[i + 1]) << 8);
            if !(0xdc00..0xe000).contains(&low) {
                return None;
            }
            let code_point =
                0x1_0000 + ((u32::from(unit - 0xd800) << 10) | u32::from(low - 0xdc00));
            out.push(char::from_u32(code_point)?);
            i += 2;
        } else if (0xdc00..0xe000).contains(&unit) {
            return None; // lone low surrogate
        } else {
            // Non-surrogate BMP unit: always a scalar value.
            out.push(char::from_u32(u32::from(unit))?);
        }
    }
    Some(out)
}

/// JS `pirBase64Encode`: whole 3-byte groups, then the 1- or 2-byte tail,
/// which is padded only on the flush (`pad` is never set for a chunk).
fn sd_base64_encode(bytes: &[u8], chars: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 2 < bytes.len() {
        let (a, b, c) = (bytes[i], bytes[i + 1], bytes[i + 2]);
        out.push(chars[(a >> 2) as usize] as char);
        out.push(chars[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(chars[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        out.push(chars[(c & 0x3f) as usize] as char);
        i += 3;
    }
    let rest = bytes.len() - i;
    if rest == 1 {
        let a = bytes[i];
        out.push(chars[(a >> 2) as usize] as char);
        out.push(chars[((a & 0x03) << 4) as usize] as char);
        if pad {
            out.push('=');
            out.push('=');
        }
    } else if rest == 2 {
        let (a, b) = (bytes[i], bytes[i + 1]);
        out.push(chars[(a >> 2) as usize] as char);
        out.push(chars[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(chars[((b & 0x0f) << 2) as usize] as char);
        if pad {
            out.push('=');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Buffer search kernel (`__pirBufferCodec("indexOfBytes")`)
// ---------------------------------------------------------------------------
//
// Only the scan loop of the JS `pirBidirectionalIndexOf` lives here: Node's
// offset/needle/encoding resolution (and every error shape it can raise) stays
// in `extensions/api/js/buffer_global.js.txt`, which hands over the resolved
// needle bytes, the already-normalized start index, the direction and the
// 16-bit alignment (ucs2 searches only line up on even offsets). The JS loops
// stay as the fallback.

/// JS `pirBidirectionalIndexOf`'s scan: the match index, or -1. Mirrors the
/// loops exactly, including the `start === -1` backwards case (a search with
/// no room left) and its `align === 2` quirk, where `-1 - (-1 % 2)` lands on 0
/// and index 0 is still probed.
fn index_of_bytes(
    haystack: &[u8],
    needle: &[u8],
    start: i64,
    backwards: bool,
    align: usize,
) -> i64 {
    let size = needle.len();
    let len = haystack.len();
    if size == 0 || size > len {
        return -1;
    }
    let step = align.max(1) as i64;
    let mut i = start;
    // Single-byte needles (`'\n'`, `0`, `','` …) are the hot path: one
    // comparison per step, no window arithmetic.
    if size == 1 {
        let byte = needle[0];
        if backwards {
            while i >= 0 {
                let at = i as usize;
                if at < len && haystack[at] == byte {
                    return i;
                }
                i -= step;
            }
        } else {
            // This direction's start is never negative (the JS wrapper clamps
            // it), and a negative index could never match anyway. A start past
            // the end clamps to the end — the slice below would otherwise
            // panic, and the empty scan is JS's `-1` either way.
            let i = i.max(0).min(len as i64);
            if step == 1 {
                // 8 bytes per step; the scalar loop below stays for the
                // aligned (ucs2) scans, where matches must land on `step`.
                return match find_first_byte(&haystack[i as usize..], byte) {
                    Some(rel) => i + rel as i64,
                    None => -1,
                };
            }
            let mut i = i;
            while i < len as i64 {
                if haystack[i as usize] == byte {
                    return i;
                }
                i += step;
            }
        }
        return -1;
    }
    // Longer needles: compare the first byte, then the rest as one slice
    // (memcmp), which is what keeps a substring search off the per-byte path.
    let first = needle[0];
    let tail = &needle[1..];
    if backwards {
        while i >= 0 {
            let at = i as usize;
            if at + size <= len && haystack[at] == first && &haystack[at + 1..at + size] == tail {
                return i;
            }
            i -= step;
        }
    } else {
        let mut i = i.max(0);
        if step == 1 {
            let last = len - size; // inclusive: last start whose tail fits
            let mut from = i as usize;
            while from <= last {
                let Some(rel) = find_first_byte(&haystack[from..=last], first) else {
                    return -1;
                };
                let at = from + rel;
                if &haystack[at + 1..at + size] == tail {
                    return at as i64;
                }
                from = at + 1;
            }
            return -1;
        }
        while i + size as i64 <= len as i64 {
            let at = i as usize;
            if haystack[at] == first && &haystack[at + 1..at + size] == tail {
                return i;
            }
            i += step;
        }
    }
    -1
}

/// Forward search for one byte, 8 bytes per step (SWAR "has-zero" test) with
/// a scalar tail. The byte-by-byte loop this replaces runs at ~370 MB/s on
/// large buffers; the word scan is ~10x that.
fn find_first_byte(haystack: &[u8], byte: u8) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let splat = u64::from(byte) * LO;
    let mut i = 0;
    while i + 8 <= haystack.len() {
        // `i + 8 <= len` above proves every read in range; `from_fn` keeps the
        // window slice-free (the hygiene gate forbids `.expect` in src/).
        let word = u64::from_le_bytes(std::array::from_fn(|k| haystack[i + k]));
        let x = word ^ splat;
        let m = x.wrapping_sub(LO) & !x & HI;
        if m != 0 {
            return Some(i + (m.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < haystack.len() {
        if haystack[i] == byte {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A search needle as bytes: the `Uint8Array` the native codec tier builds for
/// string needles, or the plain JS array the pure-JS encoding tier produces.
/// `None` for anything else (the JS loops then handle the search). A typed
/// array is *borrowed*, not copied: the scan runs synchronously with no JS
/// interleaved (see `typed_read`), so the needle never needs its own buffer.
fn codec_needle<'a, 'js>(value: Option<&'a Value<'js>>) -> Option<Cow<'a, [u8]>> {
    let object = value?.as_object()?;
    if let Some(view) = object.as_typed_array::<u8>() {
        // SAFETY: no JS runs while the slice is held (see `buffer_bytes`).
        return typed_read(view).map(Cow::Borrowed);
    }
    let array = object.clone().into_array()?;
    let len = array.len();
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        let number = array.get::<Value>(index).ok()?.as_number()?;
        if number.fract() != 0.0 || !(0.0..=255.0).contains(&number) {
            return None;
        }
        out.push(number as u8);
    }
    Some(Cow::Owned(out))
}

// ---------------------------------------------------------------------------
// Variadic (1..6 byte) LE/BE integer kernels
// (`__pirBufferCodec("variadicWrite")` / `("variadicRead")`)
// ---------------------------------------------------------------------------
//
// The 12 `read/write[U]Int{LE,BE}` methods keep their Node validation — and
// its inspect-shaped error text — in `buffer_global.js.txt`; only the byte
// arithmetic of the `writeVariadicInteger` / `readVariadicInteger` kernels
// moves here, replacing a per-call BigInt loop with a machine-word one. The JS
// kernels stay as the fallback, and the native side accepts only inputs it is
// certain about (plain finite numbers, byteLength 1..=6, an in-bounds offset):
// everything else answers `undefined`, so no validation message can drift and
// the JS tier produces the identical bytes.

/// Bytes of `writeVariadicInteger`: `trunc(value)` in two's complement over
/// `byte_length` bytes (JS `BigInt(Math.trunc(value))`, plus 2**bits when
/// negative, then `current & 0xffn` / `current >>= 8n` per byte).
fn write_variadic_bytes(
    buffer: &mut [u8],
    offset: usize,
    byte_length: usize,
    value: f64,
    big_endian: bool,
) {
    let bits = byte_length * 8;
    let truncated = value.trunc();
    let raw = if truncated < 0.0 {
        (truncated as i64).wrapping_add(1i64 << bits) as u64
    } else {
        truncated as u64
    };
    let low = raw & ((1u64 << bits) - 1);
    for index in 0..byte_length {
        let byte = ((low >> (8 * index)) & 0xff) as u8;
        let at = if big_endian {
            offset + byte_length - 1 - index
        } else {
            offset + index
        };
        buffer[at] = byte;
    }
}

/// Value of `readVariadicInteger`: the bytes accumulated most-significant-first
/// for either endianness, then the sign bit folded in and converted to a
/// `Number` (exact for the 48-bit maximum).
fn read_variadic_bytes(
    buffer: &[u8],
    offset: usize,
    byte_length: usize,
    signed: bool,
    big_endian: bool,
) -> f64 {
    let mut result: u64 = 0;
    for index in 0..byte_length {
        let at = if big_endian {
            offset + index
        } else {
            offset + byte_length - 1 - index
        };
        result = (result << 8) | u64::from(buffer[at]);
    }
    let bits = byte_length * 8;
    if signed && ((result >> (bits - 1)) & 1) == 1 {
        (result as i64).wrapping_sub(1i64 << bits) as f64
    } else {
        result as f64
    }
}

/// `offset * 8 + byte_length` (the JS kernels pack the two because the
/// dispatcher has no spare argument slots) back into its parts. `None` unless
/// it is a small non-negative integer with a 1..=6 byte length.
fn variadic_unpack(packed: Option<f64>) -> Option<(usize, usize)> {
    let packed = packed?;
    if !packed.is_finite() || packed.fract() != 0.0 || !(0.0..=1e15).contains(&packed) {
        return None;
    }
    let byte_length = (packed % 8.0) as usize;
    if !(1..=6).contains(&byte_length) {
        return None;
    }
    let offset = (packed - byte_length as f64) / 8.0;
    Some((offset as usize, byte_length))
}

/// The value guard shared by both variadic ops: a plain finite number whose
/// magnitude keeps the `as i64` / `as u64` casts exact (2**53). The JS
/// validators only ever hand over such values, so anything else is a wiring
/// change and goes back to the JS kernel.
fn variadic_value(value: Option<&Value<'_>>) -> Option<f64> {
    let number = value?.as_number()?;
    if !number.is_finite() || number.abs() > 9_007_199_254_740_992.0 {
        return None;
    }
    Some(number)
}

// ---------------------------------------------------------------------------
// util.inspect string escaper (`__pirBufferCodec("strEscape")`)
// ---------------------------------------------------------------------------

/// `pirStrEscape` from `extensions/api/js/util_shim.js.txt`: pick the quoting
/// character, then escape the control set, the active quote and the backslash.
/// The three JS paths (the `RegExp.test` no-op fast path, the `replace` pass
/// above 100 characters, the slice loop below) all produce this same output,
/// so the port is a single scan; the JS function stays as the fallback, which
/// is also what handles a lone surrogate (it cannot cross the host bridge as a
/// Rust `String`).
fn str_escape(text: &str) -> String {
    // `str.indexOf("'")` / `'"'` / '`' / `"${"` select 39 (single), -1 (double)
    // or -2 (backtick).
    let mut quotes = 39i8;
    if text.contains('\'') {
        if !text.contains('"') {
            quotes = -1;
        } else if !text.contains('`') && !text.contains("${") {
            quotes = -2;
        }
    }
    // The double-quote and backtick modes do not escape the single quote.
    let escape_single = quotes == 39;
    let (open, close) = match quotes {
        39 => ('\'', '\''),
        -1 => ('"', '"'),
        _ => ('`', '`'),
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(open);
    for ch in text.chars() {
        let unit = ch as u32;
        let control = unit == 0x5c || unit < 0x20 || (unit > 0x7e && unit < 0xa0);
        if control || (escape_single && unit == 0x27) {
            push_str_escape(&mut out, unit);
        } else {
            out.push(ch);
        }
    }
    out.push(close);
    out
}

/// `pirMeta[charCode]`, with the JS `pirEscapeFn` fallback for anything the
/// table does not cover: `\uXXXX`, lowercase and unpadded. The arms are the
/// table's two-byte entries (disjoint from the `\xNN` ranges).
fn push_str_escape(out: &mut String, unit: u32) {
    const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
    match unit {
        0x08 => out.push_str("\\b"),
        0x09 => out.push_str("\\t"),
        0x0a => out.push_str("\\n"),
        0x0c => out.push_str("\\f"),
        0x0d => out.push_str("\\r"),
        0x27 => out.push_str("\\'"),
        0x5c => out.push_str("\\\\"),
        0x00..=0x07 | 0x0b | 0x0e..=0x1f | 0x7f..=0x9f => {
            out.push_str("\\x");
            out.push(HEX_UPPER[((unit >> 4) & 0xf) as usize] as char);
            out.push(HEX_UPPER[(unit & 0xf) as usize] as char);
        }
        _ => {
            out.push_str("\\u");
            let mut digits = [0u8; 8];
            let mut len = 0;
            let mut value = unit;
            loop {
                digits[len] = b"0123456789abcdef"[(value & 0xf) as usize];
                value >>= 4;
                len += 1;
                if value == 0 {
                    break;
                }
            }
            for &digit in digits[..len].iter().rev() {
                out.push(digit as char);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TextEncoder native cores (`__pirNativeEncode` / `__pirNativeEncodeInto`)
// ---------------------------------------------------------------------------

fn native_text_encode<'js>(ctx: &Ctx<'js>, input: Option<&Value<'js>>) -> Result<Value<'js>> {
    let Some(text) = marshal_string(input) else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    typed_array_value(ctx.clone(), text.into_bytes())
}

/// WHATWG `encodeInto` (targetStart = 0): returns `{read, written}` where
/// `read` counts UTF-16 code units consumed; a character that would not fit
/// is left for the next call. Mirrors the JS `pirEncodeIntoUtf8`.
fn native_text_encode_into<'js>(
    ctx: &Ctx<'js>,
    source: Option<&Value<'js>>,
    target: Option<&Value<'js>>,
) -> Result<Value<'js>> {
    let Some(text) = marshal_string(source) else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    let typed = target
        .and_then(|value| value.as_object())
        .and_then(|object| object.as_typed_array::<u8>());
    let Some(typed) = typed else {
        return Err(Exception::throw_type(
            ctx,
            "The provided value is not of type '(ArrayBuffer or ArrayBufferView)'",
        ));
    };
    // SAFETY: no JS runs while the slice is held (see `buffer_bytes`).
    let Some(buffer) = typed_bytes(typed) else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    let mut written = 0usize;
    let mut read = 0usize;
    for ch in text.chars() {
        let size = ch.len_utf8();
        if written + size > buffer.len() {
            break;
        }
        let mut scratch = [0u8; 4];
        buffer[written..written + size].copy_from_slice(ch.encode_utf8(&mut scratch).as_bytes());
        written += size;
        read += ch.len_utf16();
    }
    let result = Object::new(ctx.clone())?;
    result.set("read", read as i32)?;
    result.set("written", written as i32)?;
    Ok(result.into_value())
}

/// Read-only-ish variant of [`buffer_bytes`] for a `TypedArray` reference.
#[allow(unsafe_code, clippy::mut_from_ref)]
fn typed_bytes<'x, 'js>(typed: &'x TypedArray<'js, u8>) -> Option<&'x mut [u8]> {
    let raw = typed.as_raw()?;
    // SAFETY: same contract as `buffer_bytes`.
    Some(unsafe { std::slice::from_raw_parts_mut(raw.as_ptr().cast::<u8>(), raw.len()) })
}

/// Read-only view of a `Uint8Array` argument. rquickjs marks `as_bytes`
/// unsafe for the same aliasing reason as `buffer_bytes`; the call runs with
/// the engine suspended, so no JS can observe or move the bytes.
#[allow(unsafe_code)]
fn typed_read<'x, 'js>(typed: &'x TypedArray<'js, u8>) -> Option<&'x [u8]> {
    // SAFETY: live attached typed array; no JS runs while the borrow is held.
    unsafe { typed.as_bytes() }
}

// ---------------------------------------------------------------------------
// Intl minimal pad (`__pirIntlFormat`)
// ---------------------------------------------------------------------------

/// Backs the Intl pad's formatter hot paths: `"number"` (grouped decimal,
/// mirroring the JS `NumberFormat.format` over `String(n)` semantics) and
/// `"date"` (UTC `YYYY-MM-DD HH:mm:ss`, mirroring the JS DateTimeFormat
/// approximation). `"number"` returns `undefined` outside the decimal
/// notation range (JS falls back to its `String(n)`-based path there).
fn intl_format_entry<'js>(
    ctx: Ctx<'js>,
    op: String,
    value: Opt<Value<'js>>,
    frac_min: Opt<f64>,
    frac_max: Opt<f64>,
) -> Result<Value<'js>> {
    match op.as_str() {
        "number" => {
            let n = match value.0.as_ref() {
                Some(v) => Coerced::<f64>::from_js(&ctx, v.clone())?.0,
                None => f64::NAN,
            };
            match intl_format_number(n, frac_min.0.unwrap_or(0.0), frac_max.0.unwrap_or(3.0)) {
                Some(text) => text.into_js(&ctx),
                None => Ok(Value::new_undefined(ctx.clone())),
            }
        }
        "date" => {
            let ms = match value.0.as_ref() {
                Some(v) => Coerced::<f64>::from_js(&ctx, v.clone())?.0,
                None => f64::NAN,
            };
            match intl_format_date_time(ms) {
                Some(text) => text.into_js(&ctx),
                None => Err(Exception::throw_range(&ctx, "Invalid time value")),
            }
        }
        other => Err(Exception::throw_type(
            &ctx,
            &format!("unknown __pirIntlFormat op: {other}"),
        )),
    }
}

/// Grouped decimal; `None` when `String(n)` would use exponent notation
/// (JS switches below 1e-6 / at 1e21) — the caller falls back to JS there.
fn intl_format_number(n: f64, min_frac: f64, max_frac: f64) -> Option<String> {
    if !n.is_finite() {
        return Some(js_number_str(n));
    }
    let abs = n.abs();
    if n != 0.0 && !(1e-6..1e21).contains(&abs) {
        return None;
    }
    let negative = n < 0.0;
    let mut text = js_number_str(abs);
    let min_frac = min_frac.clamp(0.0, 100.0) as usize;
    let max_frac = max_frac.clamp(0.0, 100.0).max(min_frac as f64) as usize;
    // Split in place: the integer digits stay in `text`'s buffer, where the
    // pair-of-`to_string` version copied them into a second `String` and the
    // owner was dropped immediately after.
    let (int_part, mut fraction) = match text.find('.') {
        Some(dot) => {
            let fraction = text.split_off(dot + 1);
            text.truncate(dot);
            (text, fraction)
        }
        None => (text, String::new()),
    };
    if fraction.len() > max_frac {
        fraction.truncate(max_frac);
    }
    while fraction.len() < min_frac {
        fraction.push('0');
    }
    let len = int_part.len();
    let mut grouped = String::with_capacity(len + len / 3);
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let mut result = String::new();
    if negative {
        result.push('-');
    }
    result.push_str(&grouped);
    if !fraction.is_empty() {
        result.push('.');
        result.push_str(&fraction);
    }
    Some(result)
}

/// UTC `YYYY-MM-DD HH:mm:ss` (the Intl pad's documented approximation);
/// `None` reproduces the JS `RangeError("Invalid time value")` path.
fn intl_format_date_time(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let secs = (ms as i64).div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60,
    );
    Some(format!(
        "{year}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
    ))
}

/// Days since 1970-01-01 → proleptic Gregorian (Howard Hinnant's
/// `civil_from_days`), so no time crate is needed.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
