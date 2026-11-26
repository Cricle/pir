//! Typed C-ABI entries for the WHATWG URL family and the querystring kernels.
//!
//! `url_shim.js.txt` is a pure-JS port of Node's URL/URLSearchParams; the
//! generic `__pir_host` envelope is not involved at all, so the whole parse
//! cost is QuickJS bytecode. These entries parse (and serialize) in Rust and
//! hand back live values, with the JS tier as the fallback for shapes the
//! native path refuses.
//!
//! Contract with the JS shims:
//!
//! * Every entry answers `undefined` when the argument shape is not one this
//!   tier owns (non-string, lone surrogate that cannot cross the bridge,
//!   unparseable URL). The shim then runs its own parser, which keeps Node's
//!   exact `ERR_INVALID_URL` / `ERR_INVALID_URI` behaviour.
//! * No entry throws for bad input; only engine failures propagate as
//!   `rquickjs::Result`.
//! * `__pirUrlParse` returns the eleven string components as a plain object,
//!   so the shim never re-walks the input text.

use rquickjs::prelude::Opt;
use rquickjs::{Array, Ctx, Object, Result, Value};

use crate::pir_node::modules::url as native;

/// JS string → Rust string; `None` when the value is not a string or cannot
/// cross the C-ABI (lone surrogate) — the caller then answers `undefined`.
fn string_arg(value: &Value<'_>) -> Option<String> {
    value.as_string()?.to_string().ok()
}

fn undefined<'js>(ctx: &Ctx<'js>) -> Value<'js> {
    Value::new_undefined(ctx.clone())
}

/// The `(input, base)` string pair or `None` for any other shape. A base of
/// `undefined` is "no base"; anything non-string is not this tier's.
fn input_pair<'js>(input: &Value<'js>, base: &Opt<Value<'js>>) -> Option<(String, Option<String>)> {
    let input = string_arg(input)?;
    let base = match base.0.as_ref() {
        None => None,
        Some(value) if value.is_undefined() => None,
        Some(value) => Some(string_arg(value)?),
    };
    Some((input, base))
}

/// `__pirUrlParse(input[, base])` — returns the eleven components as a live
/// object. `undefined` when the shape is not a string pair or the input does
/// not parse; the JS tier then runs its own parser (and its Node-exact error
/// path).
pub(super) fn url_parse<'js>(
    ctx: Ctx<'js>,
    input: Value<'js>,
    base: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some((input, base)) = input_pair(&input, &base) else {
        return Ok(undefined(&ctx));
    };
    let Ok(parts) = native::parse(&input, base.as_deref()) else {
        return Ok(undefined(&ctx));
    };

    let out = Object::new(ctx.clone())?;
    out.set("href", parts.href)?;
    out.set("protocol", parts.protocol)?;
    out.set("username", parts.username)?;
    out.set("password", parts.password)?;
    out.set("host", parts.host)?;
    out.set("hostname", parts.hostname)?;
    out.set("port", parts.port)?;
    out.set("pathname", parts.pathname)?;
    out.set("search", parts.search)?;
    out.set("hash", parts.hash)?;
    out.set("origin", parts.origin)?;
    Ok(out.into_value())
}

/// `__pirUrlPayload(input[, base])` — the same parse, but the eleven
/// components are joined by U+0000 and cross the C-ABI as one string (one
/// string conversion instead of eleven property writes; the shim splits it
/// into the URL state array). `undefined` for a non-string pair or a parse
/// failure.
pub(super) fn url_payload<'js>(
    ctx: Ctx<'js>,
    input: Value<'js>,
    base: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some((input, base)) = input_pair(&input, &base) else {
        return Ok(undefined(&ctx));
    };
    match native::parse_payload(&input, base.as_deref()) {
        Ok(text) => rquickjs::IntoJs::into_js(text, &ctx),
        Err(_) => Ok(undefined(&ctx)),
    }
}

/// `__pirUrlSearchParams(text)` — `application/x-www-form-urlencoded` parse
/// into a flat `[k1, v1, k2, v2, ...]` array, order and duplicates preserved.
/// `undefined` for a non-string (the JS constructor keeps its primitive
/// coercion and iterable/record branches).
pub(super) fn url_search_params<'js>(ctx: Ctx<'js>, text: Value<'js>) -> Result<Value<'js>> {
    let Some(text) = string_arg(&text) else {
        return Ok(undefined(&ctx));
    };
    let pairs = native::parse_form_pairs(&text);
    let out = Array::new(ctx.clone())?;
    let mut index = 0usize;
    for (name, value) in pairs {
        out.set(index, name)?;
        out.set(index + 1, value)?;
        index += 2;
    }
    Ok(out.into_value())
}

fn optional_string(value: Option<&Value<'_>>, default: &str) -> Option<String> {
    match value {
        None => Some(default.to_owned()),
        Some(value) if value.is_undefined() => Some(default.to_owned()),
        Some(value) => string_arg(value),
    }
}

/// `__pirQueryParse(text, sep, eq, maxKeys)` — Node's default-decoder
/// `querystring.parse` state machine. Returns the decoded flat
/// `[k1, v1, ...]` array; the shim builds the null-prototype object (and its
/// array-for-duplicates shape) exactly as before.
fn query_args<'js>(
    text: &Value<'js>,
    sep: &Opt<Value<'js>>,
    eq: &Opt<Value<'js>>,
    max_keys: &Opt<Value<'js>>,
) -> Option<(String, String, String, f64)> {
    let text = string_arg(text)?;
    let sep = optional_string(sep.0.as_ref(), "&")?;
    let eq = optional_string(eq.0.as_ref(), "=")?;
    let max_keys = match max_keys.0.as_ref() {
        None => 1000.0,
        Some(value) if value.is_undefined() => 1000.0,
        Some(value) => value.as_number()?,
    };
    Some((text, sep, eq, max_keys))
}

pub(super) fn query_parse<'js>(
    ctx: Ctx<'js>,
    text: Value<'js>,
    sep: Opt<Value<'js>>,
    eq: Opt<Value<'js>>,
    max_keys: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some((text, sep, eq, max_keys)) = query_args(&text, &sep, &eq, &max_keys) else {
        return Ok(undefined(&ctx));
    };

    let pairs = native::query_parse(&text, &sep, &eq, max_keys);
    let out = Array::new(ctx.clone())?;
    let mut index = 0usize;
    for (key, value) in pairs {
        out.set(index, key)?;
        out.set(index + 1, value)?;
        index += 2;
    }
    Ok(out.into_value())
}

/// Marshal a JS `Array<string>` into a flat Rust vector. `None` when the value
/// is not an array or an element cannot cross the bridge (non-string, lone
/// surrogate) — the caller then answers `undefined`.
fn flat_strings<'js>(value: &Value<'js>) -> Option<Vec<String>> {
    let array = value.as_array()?;
    let mut flat = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        let item = array.get::<rquickjs::String>(index).ok()?;
        flat.push(item.to_string().ok()?);
    }
    Some(flat)
}

/// `__pirQueryStringify(pairs, sep, eq)` — flat `[k1, v1, ...]` of already
/// stringified components; returns the escaped query string, or `undefined`
/// when the shim should keep its JS escaper (non-string element, lone
/// surrogate).
pub(super) fn query_stringify<'js>(
    ctx: Ctx<'js>,
    pairs: Value<'js>,
    sep: Opt<Value<'js>>,
    eq: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some(sep) = optional_string(sep.0.as_ref(), "&") else {
        return Ok(undefined(&ctx));
    };
    let Some(eq) = optional_string(eq.0.as_ref(), "=") else {
        return Ok(undefined(&ctx));
    };
    let Some(flat) = flat_strings(&pairs) else {
        return Ok(undefined(&ctx));
    };
    match native::query_stringify(&flat, &sep, &eq) {
        Some(text) => rquickjs::IntoJs::into_js(text, &ctx),
        None => Ok(undefined(&ctx)),
    }
}

/// `__pirUrlSearchParamsStringify(pairs)` — flat `[k1, v1, ...]` to the
/// `application/x-www-form-urlencoded` serialization (`URLSearchParams.
/// toString`). `undefined` keeps the shim's JS encoder for shapes it cannot
/// marshal.
pub(super) fn url_search_params_stringify<'js>(
    ctx: Ctx<'js>,
    pairs: Value<'js>,
) -> Result<Value<'js>> {
    let Some(flat) = flat_strings(&pairs) else {
        return Ok(undefined(&ctx));
    };
    match native::form_stringify(&flat) {
        Some(text) => rquickjs::IntoJs::into_js(text, &ctx),
        None => Ok(undefined(&ctx)),
    }
}

/// Install the URL fast-path globals.
pub(super) fn register_url_ops(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();
    globals.set("__pirUrlParse", rquickjs::prelude::Func::from(url_parse))?;
    globals.set(
        "__pirUrlPayload",
        rquickjs::prelude::Func::from(url_payload),
    )?;
    globals.set(
        "__pirUrlSearchParams",
        rquickjs::prelude::Func::from(url_search_params),
    )?;
    globals.set(
        "__pirUrlSearchParamsStringify",
        rquickjs::prelude::Func::from(url_search_params_stringify),
    )?;
    globals.set(
        "__pirQueryParse",
        rquickjs::prelude::Func::from(query_parse),
    )?;
    globals.set(
        "__pirQueryStringify",
        rquickjs::prelude::Func::from(query_stringify),
    )?;
    Ok(())
}
