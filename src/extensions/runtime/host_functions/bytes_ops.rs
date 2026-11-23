//! Typed C-ABI entries for the byte-moving fs, crypto and zlib kernels.
//!
//! `__pirNode.invoke` marshals its payload through `serde_json::Value`, so a
//! byte argument has to arrive base64-encoded inside a string field. For
//! `fs.readFileSync` of a large file, `crypto.createHash().update(...)` and
//! `zlib.deflateSync/inflateSync` that base64 lap is the dominant cost —
//! measured at ~1.3 ms/MiB per leg on top of the actual `memcpy`/digest/codec
//! work. These entries hand the `Uint8Array` backing store straight to
//! `pir-node` and return a fresh `Uint8Array`, so only the payload object
//! (path, algorithm, zlib options) crosses as JSON.
//!
//! Contract with the JS shims:
//!
//! - Every entry answers `undefined` when the argument shape is not one this
//!   tier owns (non-string path, non-byte-view data, unsupported algorithm,
//!   missing userdata). The shim then runs its `__pirNode.invoke`/pure-JS
//!   branch, so no behaviour depends on this tier existing.
//! - A kernel failure after the shape was accepted is *thrown* as the same
//!   `Error` shape `node_call_helper.js.txt` reconstructs (`code`, `syscall`,
//!   `path`, `errno`). It must not fall back: a write can have partially
//!   landed, and re-running it would duplicate bytes in append mode.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rquickjs::prelude::Opt;
use rquickjs::{Constructor, Ctx, Object, Result, String as JsString, Type, TypedArray, Value};

use super::NodeState;
use crate::pir_node::modules::crypto::{HashState, HmacState};

/// Fetch the Node runtime, cloning the `Arc` out of userdata (the guard is
/// dropped before any kernel runs: those take their own locks).
pub(super) fn runtime_of(ctx: &Ctx<'_>) -> Option<Arc<crate::pir_node::NodeRuntime>> {
    let state = ctx.userdata::<NodeState>()?;
    let runtime = state.0.clone();
    drop(state);
    Some(runtime)
}

/// JS string → Rust string; `None` when the value cannot cross the bridge
/// (non-string, lone surrogate) — the caller then answers `undefined`.
pub(super) fn string_arg(value: &Value<'_>) -> Option<String> {
    value.as_string()?.to_string().ok()
}

/// Borrow a `Uint8Array` argument's bytes. Buffer instances are real
/// `Uint8Array`s here (the shim only patches the prototype), so the same path
/// serves both.
#[allow(unsafe_code)]
fn view_bytes<'a, 'js>(value: &'a Value<'js>) -> Option<&'a [u8]> {
    let typed = value.as_object()?.as_typed_array::<u8>()?;
    // SAFETY: live attached typed array; no JS runs while the borrow is held
    // (same contract as `buffer_ops::typed_read`).
    unsafe { typed.as_bytes() }
}

fn bytes_value<'js>(ctx: Ctx<'js>, bytes: Vec<u8>) -> Result<Value<'js>> {
    // `TypedArray::new` hands the `Vec`'s allocation to QuickJS (the engine
    // takes ownership and frees it through the Vec allocator), so a multi-MB
    // read is not copied a second time on the way into JS.
    Ok(TypedArray::<u8>::new(ctx, bytes)?.into_value())
}

/// Reproduce `node_call_helper.js.txt`'s error reconstruction from a
/// host-side kernel failure.
pub(super) fn raise(ctx: &Ctx<'_>, error: &crate::pir_node::NodeError) -> rquickjs::Error {
    let build = || -> Result<Value<'_>> {
        // Node raises `ERR_OUT_OF_RANGE` as a RangeError (sqlite reads a
        // BIGINT cell that does not fit a JS number); everything else the
        // kernels report is a plain Error with a `code`.
        let constructor = if error.code == "ERR_OUT_OF_RANGE" {
            "RangeError"
        } else {
            "Error"
        };
        let ctor: Constructor = ctx.globals().get(constructor)?;
        let object: Object = ctor.construct((error.message.as_str(),))?;
        object.set("code", error.code.as_str())?;
        if let Some(syscall) = &error.syscall {
            object.set("syscall", syscall.as_str())?;
        }
        if let Some(path) = &error.path {
            object.set("path", path.as_str())?;
        }
        if let Some(errno) = error.errno {
            object.set("errno", errno)?;
        }
        Ok(object.into_value())
    };
    match build() {
        Ok(value) => ctx.throw(value),
        Err(other) => other,
    }
}

/// `__pirFsRead(path)` → `Uint8Array` or `undefined`.
pub(super) fn fs_read<'js>(ctx: Ctx<'js>, path: Value<'js>) -> Result<Value<'js>> {
    let Some(path) = string_arg(&path) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(runtime) = runtime_of(&ctx) else {
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::fs::read_file(&runtime, &path) {
        Ok(bytes) => bytes_value(ctx, bytes),
        Err(error) => Err(raise(&ctx, &error)),
    }
}

/// `__pirFsWrite(path, bytes, append)` → byte count or `undefined`.
pub(super) fn fs_write<'js>(
    ctx: Ctx<'js>,
    path: Value<'js>,
    data: Value<'js>,
    append: Opt<bool>,
) -> Result<Value<'js>> {
    let (Some(path), Some(bytes)) = (string_arg(&path), view_bytes(&data)) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(runtime) = runtime_of(&ctx) else {
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::fs::write_file(&runtime, &path, bytes, append.0.unwrap_or(false)) {
        Ok(()) => Ok(Value::new_float(ctx, bytes.len() as f64)),
        Err(error) => Err(raise(&ctx, &error)),
    }
}

/// `outputLength` crosses as an optional JS number. The shim has already
/// validated it (Node's 0..=2^32-1 integer window); a shape this tier cannot
/// read is treated as absent so the digest's own default applies.
fn output_length_arg(value: Opt<f64>) -> Option<usize> {
    value.0.filter(|length| {
        length.is_finite()
            && *length >= 0.0
            && length.fract() == 0.0
            && *length <= 4_294_967_295.0
    }).map(|length| length as usize)
}

/// `__pirCryptoDigest(algo, bytes[, outputLength])` → `Uint8Array` or
/// `undefined`.
pub(super) fn crypto_digest<'js>(
    ctx: Ctx<'js>,
    algo: Value<'js>,
    data: Value<'js>,
    output_length: Opt<f64>,
) -> Result<Value<'js>> {
    let (Some(algo), Some(bytes)) = (string_arg(&algo), view_bytes(&data)) else {
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::crypto::hash_bytes(&algo, bytes, output_length_arg(output_length)) {
        Ok(digest) => bytes_value(ctx, digest),
        // The shim only calls this for the host-backed algorithms; anything
        // else is its own JS implementation's job (and its own error text).
        Err(_) => Ok(Value::new_undefined(ctx)),
    }
}

/// `__pirCryptoHmac(algo, key, data)` → `Uint8Array` or `undefined`.
pub(super) fn crypto_hmac<'js>(
    ctx: Ctx<'js>,
    algo: Value<'js>,
    key: Value<'js>,
    data: Value<'js>,
) -> Result<Value<'js>> {
    let (Some(algo), Some(key), Some(data)) =
        (string_arg(&algo), view_bytes(&key), view_bytes(&data))
    else {
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::crypto::hmac_bytes(&algo, key, data) {
        Ok(digest) => bytes_value(ctx, digest),
        Err(_) => Ok(Value::new_undefined(ctx)),
    }
}

/// Render a finished digest for `digest(encoding)`. The three text encodings
/// become a JS string host-side; all else is the raw `Uint8Array`.
fn digest_value<'js>(
    ctx: Ctx<'js>,
    bytes: Vec<u8>,
    encoding: Option<&str>,
) -> Result<Value<'js>> {
    match encoding {
        Some("hex") => js_text(&ctx, crate::pir_node::modules::crypto::encode_hex(&bytes)),
        Some("base64") => js_text(&ctx, crate::pir_node::modules::crypto::encode_base64(&bytes)),
        Some("base64url") => js_text(&ctx, crate::pir_node::modules::crypto::encode_base64url(&bytes)),
        _ => bytes_value(ctx, bytes),
    }
}

/// `__pirCryptoDigestEnc(algo, bytes, encoding[, outputLength])` → digest as
/// `Uint8Array` or (for the text encodings) a string. The one-`update()` fast
/// path: unlike the handle trio this neither allocates a session nor finalizes
/// a second call.
pub(super) fn crypto_digest_enc<'js>(
    ctx: Ctx<'js>,
    algo: Value<'js>,
    data: Value<'js>,
    encoding: Opt<String>,
    output_length: Opt<f64>,
) -> Result<Value<'js>> {
    let (Some(algo), Some(bytes)) = (string_arg(&algo), view_bytes(&data)) else {
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::crypto::hash_bytes(&algo, bytes, output_length_arg(output_length)) {
        Ok(digest) => digest_value(ctx, digest, encoding.0.as_deref()),
        Err(_) => Ok(Value::new_undefined(ctx)),
    }
}

/// `__pirCryptoHmacEnc(algo, key, data, encoding)` — HMAC counterpart of
/// [`crypto_digest_enc`].
pub(super) fn crypto_hmac_enc<'js>(
    ctx: Ctx<'js>,
    algo: Value<'js>,
    key: Value<'js>,
    data: Value<'js>,
    encoding: Opt<String>,
) -> Result<Value<'js>> {
    let (Some(algo), Some(key), Some(data)) =
        (string_arg(&algo), view_bytes(&key), view_bytes(&data))
    else {
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::crypto::hmac_bytes(&algo, key, data) {
        Ok(digest) => digest_value(ctx, digest, encoding.0.as_deref()),
        Err(_) => Ok(Value::new_undefined(ctx)),
    }
}


/// `__pirCryptoRandom(n)` → `Uint8Array` or `undefined`.
pub(super) fn crypto_random<'js>(ctx: Ctx<'js>, n: f64) -> Result<Value<'js>> {
    if !n.is_finite() || n < 0.0 || n > (1u64 << 30) as f64 {
        return Ok(Value::new_undefined(ctx));
    }
    match crate::pir_node::modules::crypto::random_bytes(n as usize) {
        Ok(bytes) => bytes_value(ctx, bytes),
        Err(_) => Ok(Value::new_undefined(ctx)),
    }
}

// ---------------------------------------------------------------------------
// Streaming hash/HMAC handles
// ---------------------------------------------------------------------------
//
// `Hash`/`Hmac` are stateful in Node: `update()` feeds the digest state and
// `digest()` finalizes it. The shim used to buffer every chunk in JS and call
// the one-shot `__pirCryptoDigest`/`__pirCryptoHmac` at the end, which copied
// each chunk (`Buffer.from`) and concatenated all of them before the digest.
// These entries keep the state in Rust instead and hand the shim an integer
// handle, so `update()` only moves each chunk once and `digest()` is still one
// host call.
//
// The registry lives in context userdata: one per QuickJS runtime, so a
// runtime teardown drops every outstanding session. Handles are never reused
// (`next` only grows), which keeps a late `FinalizationRegistry` callback from
// releasing an unrelated later session.

#[derive(Clone)]
enum Session {
    Hash(HashState),
    Hmac(HmacState),
}

impl Session {
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Hash(state) => state.update(data),
            Self::Hmac(state) => state.update(data),
        }
    }

    fn finish(self, output_length: Option<usize>) -> Vec<u8> {
        match self {
            Self::Hash(state) => state.finish(output_length),
            Self::Hmac(state) => state.finish(),
        }
    }
}

#[derive(Default)]
struct HashRegistryInner {
    next: u64,
    entries: HashMap<u64, Session>,
}

struct HashRegistry(RefCell<HashRegistryInner>);

#[allow(unsafe_code)]
unsafe impl<'js> rquickjs::JsLifetime<'js> for HashRegistry {
    type Changed<'to> = HashRegistry;
}

/// Handles cross as JS numbers; only non-negative safe integers address a
/// session.
fn parse_handle(handle: f64) -> Option<u64> {
    if handle.is_finite()
        && handle >= 0.0
        && handle.fract() == 0.0
        && handle <= 9007199254740991.0
    {
        Some(handle as u64)
    } else {
        None
    }
}

fn insert_session(ctx: &Ctx<'_>, session: Session) -> Option<u64> {
    let guard = ctx.userdata::<HashRegistry>()?;
    let mut registry = guard.0.borrow_mut();
    let handle = registry.next;
    registry.next = registry.next.checked_add(1)?;
    registry.entries.insert(handle, session);
    Some(handle)
}

fn take_session(ctx: &Ctx<'_>, handle: u64) -> Option<Session> {
    let guard = ctx.userdata::<HashRegistry>()?;
    let mut registry = guard.0.borrow_mut();
    registry.entries.remove(&handle)
}

fn js_text<'js>(ctx: &Ctx<'js>, text: String) -> Result<Value<'js>> {
    Ok(JsString::from_str(ctx.clone(), &text)?.into_value())
}

/// `__pirCryptoHashNew(algo)` → handle or `undefined` (unsupported algorithm).
pub(super) fn crypto_hash_new<'js>(ctx: Ctx<'js>, algo: Value<'js>) -> Result<Value<'js>> {
    let Some(algo) = string_arg(&algo) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(state) = HashState::new(&algo) else {
        return Ok(Value::new_undefined(ctx));
    };
    match insert_session(&ctx, Session::Hash(state)) {
        Some(handle) => Ok(Value::new_float(ctx, handle as f64)),
        None => Ok(Value::new_undefined(ctx)),
    }
}

/// `__pirCryptoHmacNew(algo, key)` → handle or `undefined`.
pub(super) fn crypto_hmac_new<'js>(
    ctx: Ctx<'js>,
    algo: Value<'js>,
    key: Value<'js>,
) -> Result<Value<'js>> {
    let (Some(algo), Some(key)) = (string_arg(&algo), view_bytes(&key)) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(state) = HmacState::new(&algo, key) else {
        return Ok(Value::new_undefined(ctx));
    };
    match insert_session(&ctx, Session::Hmac(state)) {
        Some(handle) => Ok(Value::new_float(ctx, handle as f64)),
        None => Ok(Value::new_undefined(ctx)),
    }
}

/// `__pirCryptoHashUpdate(handle, bytes)` → `true` when the chunk was fed to
/// the Rust state, `false` when the handle/shape is not ours (the shim then
/// keeps its buffered fallback).
pub(super) fn crypto_hash_update<'js>(
    ctx: Ctx<'js>,
    handle: f64,
    data: Value<'js>,
) -> Result<bool> {
    let (Some(handle), Some(bytes)) = (parse_handle(handle), view_bytes(&data)) else {
        return Ok(false);
    };
    let Some(guard) = ctx.userdata::<HashRegistry>() else {
        return Ok(false);
    };
    let mut registry = guard.0.borrow_mut();
    match registry.entries.get_mut(&handle) {
        Some(session) => {
            session.update(bytes);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// `__pirCryptoHashCopy(handle)` → a new handle over a cloned state, or
/// `undefined` for an unknown handle. Used by `Hash.copy()`.
pub(super) fn crypto_hash_copy<'js>(ctx: Ctx<'js>, handle: f64) -> Result<Value<'js>> {
    let Some(handle) = parse_handle(handle) else {
        return Ok(Value::new_undefined(ctx));
    };
    let clone = {
        let Some(guard) = ctx.userdata::<HashRegistry>() else {
            return Ok(Value::new_undefined(ctx));
        };
        let registry = guard.0.borrow();
        registry.entries.get(&handle).cloned()
    };
    match clone {
        Some(session) => match insert_session(&ctx, session) {
            Some(handle) => Ok(Value::new_float(ctx, handle as f64)),
            None => Ok(Value::new_undefined(ctx)),
        },
        None => Ok(Value::new_undefined(ctx)),
    }
}

/// `__pirCryptoHashFinal(handle, encoding[, outputLength])` → the digest,
/// consuming the session. `encoding` is the fast-path spelling the shim chose:
/// `"hex"`, `"base64"` and `"base64url"` come back as a JS string with no
/// Buffer in between; anything else (`undefined`/`"buffer"`/null) comes back
/// as the raw `Uint8Array`, which the shim's `pirDigestResult` then renders.
/// `outputLength` is Node's XOF length for SHAKE (absent for every other
/// digest).
pub(super) fn crypto_hash_final<'js>(
    ctx: Ctx<'js>,
    handle: f64,
    encoding: Opt<String>,
    output_length: Opt<f64>,
) -> Result<Value<'js>> {
    let Some(handle) = parse_handle(handle) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(session) = take_session(&ctx, handle) else {
        return Ok(Value::new_undefined(ctx));
    };
    let bytes = session.finish(output_length_arg(output_length));
    match encoding.0.as_deref() {
        Some("hex") => js_text(&ctx, crate::pir_node::modules::crypto::encode_hex(&bytes)),
        Some("base64") => js_text(&ctx, crate::pir_node::modules::crypto::encode_base64(&bytes)),
        Some("base64url") => js_text(&ctx, crate::pir_node::modules::crypto::encode_base64url(&bytes)),
        _ => bytes_value(ctx, bytes),
    }
}

/// `__pirCryptoHashRelease(handle)` — explicit/GC release path. Returns
/// `undefined` whether or not the handle still existed.
pub(super) fn crypto_hash_release(ctx: Ctx<'_>, handle: f64) -> Result<()> {
    if let Some(handle) = parse_handle(handle) {
        let _ = take_session(&ctx, handle);
    }
    Ok(())
}

/// `__pirCryptoRandomUUID()` → a v4 UUID string built entirely in Rust.
pub(super) fn crypto_random_uuid<'js>(ctx: Ctx<'js>) -> Result<Value<'js>> {
    js_text(&ctx, crate::pir_node::modules::crypto::new_uuid())
}

// ---------------------------------------------------------------------------
// zlib one-shot codecs
// ---------------------------------------------------------------------------

/// The ops this tier serves. Brotli/zstd stay on the JSON envelope: their
/// option plumbing (`params`, quality/lgwin) is richer than the typed shape.
const ZLIB_TYPED_OPS: [&str; 12] = [
    "gzip",
    "gzipSync",
    "gunzip",
    "gunzipSync",
    "deflate",
    "deflateSync",
    "inflate",
    "inflateSync",
    "deflateRaw",
    "deflateRawSync",
    "inflateRaw",
    "inflateRawSync",
];

/// One optional numeric option. Outer `None` = a shape this tier cannot serve
/// exactly (fall back to the envelope for Node's own error text); inner `None`
/// = absent/`NaN`, both of which mean "use the codec default" (Node's range
/// checks pass `NaN`, and the envelope's integer reader ignores it).
fn number_option<'js>(
    object: &Object<'js>,
    key: &str,
    min: f64,
    max: f64,
) -> Result<Option<Option<f64>>> {
    let value: Value = object.get(key)?;
    match value.type_of() {
        Type::Undefined | Type::Uninitialized => Ok(Some(None)),
        Type::Int | Type::Float => {
            let number = value.as_number().unwrap_or(f64::NAN);
            if number.is_nan() {
                return Ok(Some(None));
            }
            if !number.is_finite() || number < min || number > max {
                return Ok(None);
            }
            Ok(Some(Some(number)))
        }
        _ => Ok(None),
    }
}

/// `options.dictionary` as a live value. Only a `Uint8Array` (Buffer
/// included) is borrowed zero-copy; an `ArrayBuffer` or another view kind is
/// the envelope's shape, so this tier declines it (outer `None`).
fn dictionary_option<'js>(object: &Object<'js>) -> Result<Option<Option<Value<'js>>>> {
    let value: Value = object.get("dictionary")?;
    match value.type_of() {
        Type::Undefined | Type::Null | Type::Uninitialized => Ok(Some(None)),
        Type::Object => {
            if value
                .as_object()
                .and_then(|object| object.as_typed_array::<u8>())
                .is_none()
            {
                return Ok(None);
            }
            Ok(Some(Some(value)))
        }
        _ => Ok(None),
    }
}

/// `__pirZlib(op, data, options?)` → `Uint8Array` or `undefined`.
///
/// The byte-level counterpart of the `zlib` envelope op: `data` is borrowed
/// from the JS typed array and the result `Vec` is handed to QuickJS without a
/// copy, so a multi-MB round-trip pays no base64 on either leg. `op` accepts
/// the host-table names (`deflate`, `gzip`, …) and the JS `*Sync` spellings.
///
/// Shape policy (same as the rest of this file): anything this tier cannot
/// serve *exactly* — an unknown op, a non-view chunk, a non-object options
/// value, a dictionary that is not a `Uint8Array`, an option whose type/range
/// the envelope validator owns — answers `undefined` so the shim's envelope
/// call raises Node's own error wording. A codec failure after acceptance is
/// thrown (`Z_DATA_ERROR`/`Z_BUF_ERROR`/…) and never falls back: decompression
/// is side-effect free, but a silent second attempt would double the work.
pub(super) fn zlib<'js>(
    ctx: Ctx<'js>,
    op: Value<'js>,
    data: Value<'js>,
    options: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some(op) = string_arg(&op) else {
        return Ok(Value::new_undefined(ctx));
    };
    if !ZLIB_TYPED_OPS.contains(&op.as_str()) {
        return Ok(Value::new_undefined(ctx));
    }
    let mut level = None;
    let mut window_bits = None;
    let mut max_output = None;
    let mut finish_flush = None;
    let mut dictionary_value: Option<Value<'js>> = None;
    if let Some(options) = options.0.as_ref() {
        match options.type_of() {
            Type::Undefined | Type::Null | Type::Uninitialized => {}
            Type::Object => {
                let Some(object) = options.as_object() else {
                    return Ok(Value::new_undefined(ctx));
                };
                match number_option(object, "level", -1.0, 9.0)? {
                    Some(value) => level = value.map(|number| number as i32),
                    None => return Ok(Value::new_undefined(ctx)),
                }
                match number_option(object, "windowBits", 8.0, 15.0)? {
                    Some(value) => window_bits = value.map(|number| number as i32),
                    None => return Ok(Value::new_undefined(ctx)),
                }
                match number_option(object, "maxOutputLength", 1.0, 9_007_199_254_740_991.0)? {
                    Some(value) => max_output = value,
                    None => return Ok(Value::new_undefined(ctx)),
                }
                // Node's finishFlush constants are 0..=5; anything else stays
                // with the envelope's own validation.
                match number_option(object, "finishFlush", 0.0, 5.0)? {
                    Some(value) => finish_flush = value.map(|number| number as i32),
                    None => return Ok(Value::new_undefined(ctx)),
                }
                match dictionary_option(object)? {
                    Some(value) => dictionary_value = value,
                    None => return Ok(Value::new_undefined(ctx)),
                }
            }
            // A primitive/function options value stays with the envelope.
            _ => return Ok(Value::new_undefined(ctx)),
        }
    }

    // Take the input view only now: every read above goes through
    // `Object::get`, which can run a user getter that detaches or resizes the
    // backing store. `TypedArray::as_bytes` requires that no JS runs while the
    // slice is alive, and nothing does between this point and the codec call.
    let Some(data) = view_bytes(&data) else {
        return Ok(Value::new_undefined(ctx));
    };
    let dictionary = dictionary_value
        .as_ref()
        .and_then(|value| view_bytes(value));
    let options = crate::pir_node::modules::child::ZlibBytesOptions {
        level,
        window_bits,
        dictionary,
        max_output_length: max_output,
        finish_flush,
        reject_garbage: false,
    };
    match crate::pir_node::modules::child::zlib_bytes_op(&op, data, &options) {
        Ok(bytes) => bytes_value(ctx, bytes),
        Err(error) => Err(raise(&ctx, &error)),
    }
}

/// Install the byte/crypto globals consumed by the fs/crypto shims.
pub(super) fn register_bytes_ops(ctx: &Ctx<'_>) -> Result<()> {
    ctx.store_userdata(HashRegistry(RefCell::new(HashRegistryInner::default())))?;
    let globals = ctx.globals();
    globals.set("__pirFsRead", rquickjs::prelude::Func::from(fs_read))?;
    globals.set("__pirFsWrite", rquickjs::prelude::Func::from(fs_write))?;
    globals.set(
        "__pirCryptoDigest",
        rquickjs::prelude::Func::from(crypto_digest),
    )?;
    globals.set("__pirCryptoHmac", rquickjs::prelude::Func::from(crypto_hmac))?;
    globals.set(
        "__pirCryptoRandom",
        rquickjs::prelude::Func::from(crypto_random),
    )?;
    globals.set(
        "__pirCryptoHashNew",
        rquickjs::prelude::Func::from(crypto_hash_new),
    )?;
    globals.set(
        "__pirCryptoDigestEnc",
        rquickjs::prelude::Func::from(crypto_digest_enc),
    )?;
    globals.set(
        "__pirCryptoHmacEnc",
        rquickjs::prelude::Func::from(crypto_hmac_enc),
    )?;
    globals.set(
        "__pirCryptoHmacNew",
        rquickjs::prelude::Func::from(crypto_hmac_new),
    )?;
    globals.set(
        "__pirCryptoHashUpdate",
        rquickjs::prelude::Func::from(crypto_hash_update),
    )?;
    globals.set(
        "__pirCryptoHashCopy",
        rquickjs::prelude::Func::from(crypto_hash_copy),
    )?;
    globals.set(
        "__pirCryptoHashFinal",
        rquickjs::prelude::Func::from(crypto_hash_final),
    )?;
    globals.set(
        "__pirCryptoHashRelease",
        rquickjs::prelude::Func::from(crypto_hash_release),
    )?;
    globals.set(
        "__pirCryptoRandomUUID",
        rquickjs::prelude::Func::from(crypto_random_uuid),
    )?;
    // `__pirZlib` and `__pirFsRead`/`__pirFsWrite` are the same tier: bytes in,
    // bytes out, no base64 lap. The zlib shim tries it first for `*Sync` and
    // falls back to the envelope for shapes this tier declines.
    globals.set("__pirZlib", rquickjs::prelude::Func::from(zlib))?;
    Ok(())
}
