//! Typed C-ABI entry for `util.inspect`'s default fast path.
//!
//! The shim's `pirInspect` is a faithful port of Node's `inspect.js` and stays
//! the source of truth; this entry renders the common shapes (primitives,
//! plain/null-prototype objects, dense arrays, `Map`, `Set`, `Date`, `RegExp`
//! and named class instances) directly from live JS values and returns
//! `undefined` (the JS value, not the string `"undefined"`) for everything
//! else so the shim falls back.
//!
//! The layout maths mirror `reduceToSingleString` / `isBelowBreakLength` /
//! `groupArrayElements` in `util_shim.js.txt` for the default option set
//! (`depth` may vary, `compact: 3`, `breakLength: 80`, no colours, no hidden
//! properties). Any shape whose rendering we cannot reproduce byte-for-byte
//! (accessors, proxies, custom `util.inspect` symbols, sparse/extended arrays,
//! boxed primitives, typed arrays, errors, subclasses of built-ins, …) is
//! delegated.
//!
//! Only the operations rquickjs does not expose safely are raw QuickJS calls:
//! descriptor reads (`JS_GetOwnProperty`, so accessors are visible without
//! invoking getters), own-key enumeration (atoms keep their symbol identity),
//! intrinsic-class prototypes, class ids, `JS_ToInt64` and one `JS_Call` with a
//! custom `this`. Each is wrapped in the helper that owns the `SAFETY` note;
//! everything else uses `Object`/`Array`/`Value`/`Atom`/`Coerced` APIs.

#![allow(unsafe_code)]

use rquickjs::atom::PredefinedAtom;
use rquickjs::prelude::Opt;
use rquickjs::{qjs, Atom, Coerced, Ctx, IntoJs, Object, Result, Symbol, Value};

/// Default `breakLength`.
const BREAK_LENGTH: i64 = 80;
/// Default `compact`.
const COMPACT: i64 = 3;
/// Default `maxArrayLength`.
const MAX_ARRAY_LENGTH: usize = 100;
/// Default `maxStringLength` (longer strings are delegated).
const MAX_STRING_LENGTH: usize = 10_000;
/// Hard recursion guard; deeper renders delegate to the JS formatter.
const MAX_RENDER_DEPTH: i64 = 256;

// QuickJS class ids (see the enum at the top of quickjs.c in the pinned
// rquickjs-sys 0.14.0 union). Only ordinary objects reach the generic path.
const CLASS_OBJECT: u32 = 1;
const CLASS_ARRAY: u32 = 2;
const CLASS_DATE: u32 = 10;
const CLASS_REGEXP: u32 = 19;
const CLASS_MAP: u32 = 36;
const CLASS_SET: u32 = 37;

/// `JS_PROP_GETSET`: the descriptor describes an accessor.
const PROP_GETSET: i32 = 16;

/// A JS atom we own (duplicated out of a property enumeration).
struct AtomGuard {
    ctx: *mut qjs::JSContext,
    atom: qjs::JSAtom,
}

impl Drop for AtomGuard {
    fn drop(&mut self) {
        // SAFETY: the atom was duplicated by `enum_keys` (`JS_DupAtom`) and is
        // owned by this guard; the context outlives the guard because the
        // enumeration borrows the same `Ctx`.
        unsafe { qjs::JS_FreeAtom(self.ctx, self.atom) }
    }
}

/// A property name as seen by inspect: a string key (UTF-16 code units) or a
/// symbol description (UTF-16 code units).
enum Key {
    Str(Vec<u16>),
    Sym(Vec<u16>),
}

enum Prop<'js> {
    /// The property does not exist.
    Absent,
    /// The property exists but is an accessor (getter and/or setter).
    Accessor,
    /// The property is a data property; the value is owned and freed on drop.
    Data(Value<'js>),
}

#[inline]
fn is_exception(value: qjs::JSValue) -> bool {
    value.tag == qjs::JS_TAG_EXCEPTION as i64
}

/// Retrieve and drop the pending exception so the context stays usable.
#[inline]
fn clear_error(ctx: &Ctx<'_>) {
    drop(ctx.catch());
}

/// `Object.prototype` / `Array.prototype` / … for a class id, owned.
fn class_proto<'js>(ctx: &Ctx<'js>, class_id: u32) -> Option<Value<'js>> {
    // SAFETY: `JS_GetClassProto` returns a new reference to the runtime's
    // intrinsic prototype (no JS runs); ownership is transferred into the
    // returned `Value`, which releases it on drop.
    unsafe {
        let raw = qjs::JS_GetClassProto(ctx.as_raw().as_ptr(), class_id);
        if is_exception(raw) {
            clear_error(ctx);
            return None;
        }
        Some(Value::from_raw(ctx.clone(), raw))
    }
}

/// QuickJS class id of an object value (`JS_INVALID_CLASS_ID` otherwise).
fn class_id(value: &Value<'_>) -> u32 {
    // SAFETY: `JS_GetClassID` only reads the object header and returns
    // `JS_INVALID_CLASS_ID` for non-objects.
    unsafe { qjs::JS_GetClassID(value.as_raw()) }
}

/// Read a JS string as UTF-16 code units (lone surrogates included).
fn read_units(ctx: &Ctx<'_>, value: &Value<'_>) -> Option<Vec<u16>> {
    // SAFETY: `JS_ToCStringLenUTF16` returns an owned C string (freed below)
    // and a length; the slice is copied before the string is released. No JS
    // runs while the slice is alive.
    unsafe {
        let mut len: u64 = 0;
        let ptr = qjs::JS_ToCStringLenUTF16(ctx.as_raw().as_ptr(), &mut len, value.as_raw());
        if ptr.is_null() {
            clear_error(ctx);
            return None;
        }
        let units = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        qjs::JS_FreeCStringUTF16(ctx.as_raw().as_ptr(), ptr);
        Some(units)
    }
}

/// Read a number/bigint/string as text using the engine's ECMAScript
/// `ToString`.
fn scalar_text(ctx: &Ctx<'_>, value: &Value<'_>) -> Option<String> {
    match value.get::<Coerced<String>>() {
        Ok(Coerced(text)) => Some(text),
        Err(_) => {
            clear_error(ctx);
            None
        }
    }
}

/// Read a JS string as a Rust `String` when the value round-trips exactly.
///
/// The engine's UTF-8 conversion keeps unpaired surrogates as WTF-8, which is
/// invalid UTF-8 and therefore rejected here; the caller then delegates to the
/// JS formatter (which renders the surrogate escape) instead of substituting
/// U+FFFD.
fn read_string_fast(ctx: &Ctx<'_>, value: &Value<'_>) -> Option<String> {
    let string = value.as_string()?;
    match string.to_string() {
        Ok(text) => Some(text),
        Err(_) => {
            clear_error(ctx);
            None
        }
    }
}

/// Render a JS number value with `String(number)` semantics (`-0` included).
fn number_text(ctx: &Ctx<'_>, value: &Value<'_>) -> Option<String> {
    if let Some(int) = value.as_int() {
        return Some(int.to_string());
    }
    if let Some(number) = value.as_float() {
        if number == 0.0 {
            return Some(if number.is_sign_negative() {
                "-0".to_owned()
            } else {
                "0".to_owned()
            });
        }
    }
    scalar_text(ctx, value)
}

/// `Symbol.prototype.toString.call(value)`.
fn symbol_text<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Option<String> {
    let atom = match Atom::from_value(ctx.clone(), value) {
        Ok(atom) => atom,
        Err(_) => {
            clear_error(ctx);
            return None;
        }
    };
    // `Atom::to_js_string` is `JS_AtomToString`: for a symbol atom that is the
    // description (empty for a description-less local symbol), exactly what
    // the previous raw implementation read.
    let description = match atom.to_js_string() {
        Ok(description) => description,
        Err(_) => {
            clear_error(ctx);
            return None;
        }
    };
    let text = read_string_fast(ctx, &description)?;
    Some(format!("Symbol({text})"))
}

/// Names of the standard global constructors. `util.format('%s', …)` inspects
/// objects whose `toString` comes from one of these prototypes and stringifies
/// everything else; unknown owners delegate (the shim's built-in set is
/// captured from `globalThis` at load time).
/// Constructor names in Node's `builtInObjects` snapshot (the globals that
/// exist when `internal/util/inspect` is evaluated during bootstrap). `%s`
/// calls `String(value)` for everything else (`pirHasBuiltInToString`).
///
/// `Buffer` and `URLSearchParams` are deliberately absent: Node installs those
/// globals after the snapshot, so `util.format("%s", buffer)` is
/// `String(buffer)` (the decoded text), not `inspect(buffer)`.
fn is_std_constructor(name: &str) -> bool {
    matches!(
        name,
        "Object"
            | "Function"
            | "Array"
            | "Number"
            | "Boolean"
            | "String"
            | "Symbol"
            | "BigInt"
            | "Date"
            | "RegExp"
            | "Error"
            | "EvalError"
            | "RangeError"
            | "ReferenceError"
            | "SyntaxError"
            | "TypeError"
            | "URIError"
            | "AggregateError"
            | "Map"
            | "Set"
            | "WeakMap"
            | "WeakSet"
            | "WeakRef"
            | "FinalizationRegistry"
            | "Promise"
            | "ArrayBuffer"
            | "SharedArrayBuffer"
            | "DataView"
            | "Uint8Array"
            | "Int8Array"
            | "Uint8ClampedArray"
            | "Int16Array"
            | "Uint16Array"
            | "Int32Array"
            | "Uint32Array"
            | "Float32Array"
            | "Float64Array"
            | "BigInt64Array"
            | "BigUint64Array"
            | "URL"
            | "TextEncoder"
            | "TextDecoder"
            | "EventEmitter"
    )
}

/// Own `constructor` name of a prototype (data function with a non-empty name).
fn own_constructor_name<'js>(ctx: &Ctx<'js>, proto: &Value<'js>) -> Option<String> {
    let constructor = match own_prop_atom(
        ctx,
        proto.as_raw(),
        PredefinedAtom::Constructor as qjs::JSAtom,
    )? {
        Prop::Data(value) => value,
        _ => return None,
    };
    if !constructor.is_function() {
        return None;
    }
    let name = match own_prop_atom(
        ctx,
        constructor.as_raw(),
        PredefinedAtom::Name as qjs::JSAtom,
    ) {
        Some(Prop::Data(value)) => {
            if value.is_string() {
                read_string_fast(ctx, &value)
            } else {
                None
            }
        }
        _ => None,
    };
    name.filter(|name| !name.is_empty())
}

/// Convert an atom into the inspect-visible key representation.
fn atom_to_key(ctx: &Ctx<'_>, atom: qjs::JSAtom) -> Option<Key> {
    // SAFETY: `JS_AtomToValue`/`JS_AtomToString` return owned values that are
    // transferred into `Value` (or explicitly freed) inside this block; no JS
    // runs.
    unsafe {
        let raw_ctx = ctx.as_raw().as_ptr();
        let value = qjs::JS_AtomToValue(raw_ctx, atom);
        if is_exception(value) {
            clear_error(ctx);
            return None;
        }
        if qjs::JS_VALUE_GET_TAG(value) == qjs::JS_TAG_SYMBOL {
            qjs::JS_FreeValue(raw_ctx, value);
            // For a symbol atom, `JS_AtomToString` returns the description.
            let description = qjs::JS_AtomToString(raw_ctx, atom);
            if is_exception(description) {
                clear_error(ctx);
                return None;
            }
            let description = Value::from_raw(ctx.clone(), description);
            read_units(ctx, &description).map(Key::Sym)
        } else {
            let value = Value::from_raw(ctx.clone(), value);
            read_units(ctx, &value).map(Key::Str)
        }
    }
}

/// Own-property lookup without running getters.
fn own_prop_atom<'js>(
    ctx: &Ctx<'js>,
    object: qjs::JSValue,
    atom: qjs::JSAtom,
) -> Option<Prop<'js>> {
    // SAFETY: `JS_GetOwnProperty` fills the descriptor with owned references
    // (`value`/`getter`/`setter`); every one is either transferred into a
    // `Value` or freed here. It does not invoke getters or setters.
    unsafe {
        let raw_ctx = ctx.as_raw().as_ptr();
        let mut desc = qjs::JSPropertyDescriptor {
            flags: 0,
            value: qjs::JS_UNDEFINED,
            getter: qjs::JS_UNDEFINED,
            setter: qjs::JS_UNDEFINED,
        };
        let found = qjs::JS_GetOwnProperty(raw_ctx, &mut desc, object, atom);
        if found < 0 {
            clear_error(ctx);
            return None;
        }
        let flags = desc.flags;
        let value = desc.value;
        qjs::JS_FreeValue(raw_ctx, desc.getter);
        qjs::JS_FreeValue(raw_ctx, desc.setter);
        if found == 0 {
            return Some(Prop::Absent);
        }
        if flags & PROP_GETSET != 0 {
            qjs::JS_FreeValue(raw_ctx, value);
            return Some(Prop::Accessor);
        }
        Some(Prop::Data(Value::from_raw(ctx.clone(), value)))
    }
}

/// Own-property lookup for a string key, without running getters.
fn own_prop_named<'js>(ctx: &Ctx<'js>, object: qjs::JSValue, name: &str) -> Option<Prop<'js>> {
    // SAFETY: `JS_NewAtomLen` interns the key and returns an atom owned by
    // this block; it is released after `own_prop_atom` has consumed it.
    unsafe {
        let raw_ctx = ctx.as_raw().as_ptr();
        let atom = qjs::JS_NewAtomLen(
            raw_ctx,
            name.as_ptr() as *const std::ffi::c_char,
            name.len() as _,
        );
        if atom == qjs::JS_ATOM_NULL {
            clear_error(ctx);
            return None;
        }
        let prop = own_prop_atom(ctx, object, atom);
        qjs::JS_FreeAtom(raw_ctx, atom);
        prop
    }
}

/// Own-property lookup for an array index, without running getters.
fn own_prop_index<'js>(ctx: &Ctx<'js>, object: qjs::JSValue, index: u32) -> Option<Prop<'js>> {
    // SAFETY: `JS_NewAtomUInt32` returns an atom owned by this block; it is
    // released after `own_prop_atom` has consumed it.
    unsafe {
        let raw_ctx = ctx.as_raw().as_ptr();
        let atom = qjs::JS_NewAtomUInt32(raw_ctx, index);
        if atom == qjs::JS_ATOM_NULL {
            clear_error(ctx);
            return None;
        }
        let prop = own_prop_atom(ctx, object, atom);
        qjs::JS_FreeAtom(raw_ctx, atom);
        prop
    }
}

/// `ToInt64` (`JS_ToInt64`, not the BigInt-accepting `Ext` variant).
fn to_i64(ctx: &Ctx<'_>, value: &Value<'_>) -> Option<i64> {
    // SAFETY: `JS_ToInt64` only reads the value and writes the out-parameter;
    // no JS runs.
    unsafe {
        let mut converted: i64 = 0;
        let result = qjs::JS_ToInt64(ctx.as_raw().as_ptr(), &mut converted, value.as_raw());
        if result < 0 {
            clear_error(ctx);
            return None;
        }
        Some(converted)
    }
}

/// Call `function` with `this` and no arguments.
fn call_method<'js>(
    ctx: &Ctx<'js>,
    function: &Value<'js>,
    this: qjs::JSValue,
) -> Option<Value<'js>> {
    // SAFETY: `JS_Call` receives an owned reference to the function and a
    // borrowed `this` that outlives the call; the result is transferred into
    // `Value`. The argument list is empty, so the pointer is never read.
    unsafe {
        let result = qjs::JS_Call(
            ctx.as_raw().as_ptr(),
            function.as_raw(),
            this,
            0,
            std::ptr::null_mut(),
        );
        if is_exception(result) {
            clear_error(ctx);
            return None;
        }
        Some(Value::from_raw(ctx.clone(), result))
    }
}

/// Own keys (strings then symbols), optionally enumerable-only.
fn enum_keys(ctx: &Ctx<'_>, object: qjs::JSValue, enumerable_only: bool) -> Option<Vec<AtomGuard>> {
    // SAFETY: `JS_GetOwnPropertyNames` returns a table of atoms owned by the
    // table; each atom is duplicated into an `AtomGuard` and the table is
    // released before returning. No JS runs.
    unsafe {
        let raw_ctx = ctx.as_raw().as_ptr();
        let mut table: *mut qjs::JSPropertyEnum = std::ptr::null_mut();
        let mut len: u32 = 0;
        let mut flags = (qjs::JS_GPN_STRING_MASK | qjs::JS_GPN_SYMBOL_MASK) as std::ffi::c_int;
        if enumerable_only {
            flags |= qjs::JS_GPN_ENUM_ONLY as std::ffi::c_int;
        }
        let result = qjs::JS_GetOwnPropertyNames(raw_ctx, &mut table, &mut len, object, flags);
        if result < 0 {
            clear_error(ctx);
            return None;
        }
        let mut keys = Vec::with_capacity(len as usize);
        if !table.is_null() {
            for index in 0..len as usize {
                let atom = (*table.add(index)).atom;
                keys.push(AtomGuard {
                    ctx: raw_ctx,
                    atom: qjs::JS_DupAtom(raw_ctx, atom),
                });
            }
            qjs::JS_FreePropertyEnum(raw_ctx, table, len);
        }
        Some(keys)
    }
}

/// Prototype of an object value.
///
/// `Some(None)` means the object has a null prototype; the outer `None` means
/// the lookup itself threw (a proxy `getPrototypeOf` trap), so the caller must
/// delegate to the JS formatter.
fn prototype_of<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Option<Option<Value<'js>>> {
    let object = value.as_object()?;
    let proto = object.get_prototype();
    if ctx.has_exception() {
        clear_error(ctx);
        return None;
    }
    Some(proto.map(Object::into_value))
}

/// JS string width used by the layout code (`String#length`).
fn width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }
}

/// `String.prototype.padStart` / `padEnd` with spaces.
fn pad(text: &str, target: usize, start: bool) -> String {
    let current = width(text);
    if current >= target {
        return text.to_owned();
    }
    let padding = " ".repeat(target - current);
    if start {
        padding + text
    } else {
        let mut out = text.to_owned();
        out.push_str(&padding);
        out
    }
}

/// Node's `pirMeta` table entry for a single UTF-16 code unit.
fn meta(code: u16) -> String {
    match code {
        0x00..=0x07 => format!("\\x{code:02X}"),
        0x08 => "\\b".to_owned(),
        0x09 => "\\t".to_owned(),
        0x0A => "\\n".to_owned(),
        0x0B => "\\x0B".to_owned(),
        0x0C => "\\f".to_owned(),
        0x0D => "\\r".to_owned(),
        0x0E..=0x1F => format!("\\x{code:02X}"),
        0x27 => "\\'".to_owned(),
        0x5C => "\\\\".to_owned(),
        0x7F..=0x9F => format!("\\x{code:02X}"),
        _ => format!("\\u{code:x}"),
    }
}

fn push_scalar(out: &mut String, high: u16, low: u16) {
    let scalar = 0x1_0000 + (((high as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
    if let Some(ch) = char::from_u32(scalar) {
        out.push(ch);
    }
}

/// Port of `pirStrEscape` for UTF-16 input (quotes included).
fn quote_units(units: &[u16]) -> String {
    let mut quote: u16 = 0x27; // '
    if units.contains(&0x27) {
        if !units.contains(&0x22) {
            quote = 0x22; // "
        } else if !units.contains(&0x60) && !contains_dollar_brace(units) {
            quote = 0x60; // `
        }
    }
    let escape_single = quote == 0x27;

    let mut out = String::with_capacity(units.len() + 2);
    let mut index = 0;
    while index < units.len() {
        let code = units[index];
        if (code == 0x27 && escape_single)
            || code == 0x5C
            || code < 0x20
            || (code > 0x7E && code < 0xA0)
        {
            out.push_str(&meta(code));
            index += 1;
        } else if (0xD800..=0xDBFF).contains(&code)
            && index + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&units[index + 1])
        {
            push_scalar(&mut out, code, units[index + 1]);
            index += 2;
        } else if (0xD800..=0xDFFF).contains(&code) {
            out.push_str(&format!("\\u{code:x}"));
            index += 1;
        } else {
            out.push(char::from_u32(code as u32).unwrap_or('\u{FFFD}'));
            index += 1;
        }
    }

    let (open, close) = match quote {
        0x22 => ("\"", "\""),
        0x60 => ("`", "`"),
        _ => ("'", "'"),
    };
    format!("{open}{out}{close}")
}

fn contains_dollar_brace(units: &[u16]) -> bool {
    units
        .windows(2)
        .any(|pair| pair[0] == 0x24 && pair[1] == 0x7B)
}

/// `str.replace(pirStrEscapeSequencesReplacer, pirEscapeFn)` without quotes.
fn escape_no_quotes(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let code = units[index];
        if code == 0x27 || code == 0x5C || code < 0x20 || (code > 0x7E && code < 0xA0) {
            out.push_str(&meta(code));
            index += 1;
        } else if (0xD800..=0xDBFF).contains(&code)
            && index + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&units[index + 1])
        {
            push_scalar(&mut out, code, units[index + 1]);
            index += 2;
        } else if (0xD800..=0xDFFF).contains(&code) {
            out.push_str(&format!("\\u{code:x}"));
            index += 1;
        } else {
            out.push(char::from_u32(code as u32).unwrap_or('\u{FFFD}'));
            index += 1;
        }
    }
    out
}

fn is_ascii_alpha(code: u16) -> bool {
    (code >= b'a' as u16 && code <= b'z' as u16) || (code >= b'A' as u16 && code <= b'Z' as u16)
}

fn is_ascii_alnum(code: u16) -> bool {
    is_ascii_alpha(code) || (code >= b'0' as u16 && code <= b'9' as u16)
}

/// `^[a-zA-Z_][a-zA-Z_0-9]*$` over UTF-16 units (non-ASCII never matches).
fn is_identifier(units: &[u16]) -> bool {
    let mut iter = units.iter();
    match iter.next() {
        Some(&first) if is_ascii_alpha(first) || first == b'_' as u16 => {}
        _ => return false,
    }
    iter.all(|&code| is_ascii_alnum(code) || code == b'_' as u16)
}

fn units_eq_ascii(units: &[u16], text: &str) -> bool {
    units.len() == text.len() && units.iter().zip(text.bytes()).all(|(&a, b)| a == b as u16)
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// `Date.prototype.toISOString` for a finite millisecond value.
fn date_to_iso(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let total = ms as i64;
    let days = total.div_euclid(86_400_000);
    let in_day = total.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = in_day / 3_600_000;
    let minute = (in_day / 60_000) % 60;
    let second = (in_day / 1_000) % 60;
    let millis = in_day % 1_000;

    let year_text = if (0..=9_999).contains(&year) {
        format!("{year:04}")
    } else if year > 9_999 {
        format!("+{year:06}")
    } else {
        format!("-{:06}", -year)
    };
    Some(format!(
        "{year_text}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ))
}

struct Renderer<'js> {
    ctx: Ctx<'js>,
    depth: i64,
    indent: usize,
    /// Node's `ctx.currentDepth`: the depth of the most recently entered
    /// container (it is not restored when a child returns).
    last_rt: i64,
    /// Values on the current inspection path, for cycle detection.
    seen: Vec<Value<'js>>,
    custom: Symbol<'js>,
    object_proto: Value<'js>,
}

impl<'js> Renderer<'js> {
    fn new(ctx: Ctx<'js>) -> Option<Self> {
        let custom = Symbol::new_global(ctx.clone(), "nodejs.util.inspect.custom").ok()?;
        let object_proto = class_proto(&ctx, CLASS_OBJECT)?;
        Some(Renderer {
            ctx,
            depth: 2,
            indent: 0,
            last_rt: 0,
            seen: Vec::new(),
            custom,
            object_proto,
        })
    }

    /// Validate the options object; `None` means "delegate to the JS shim".
    fn parse_options(&mut self, options: Option<&Value<'js>>) -> Option<i64> {
        let Some(options) = options else {
            return Some(2);
        };
        if options.is_undefined() || options.is_null() {
            return Some(2);
        }
        if !options.is_object() || options.is_function() || options.is_proxy() {
            return None;
        }
        // `Object.keys` ignores symbol keys; only string keys matter.
        let keys = enum_keys(&self.ctx, options.as_raw(), true)?;
        let mut depth = 2i64;
        for key in &keys {
            #[allow(clippy::question_mark)]
            let Some(name) = atom_to_key(&self.ctx, key.atom) else {
                return None;
            };
            let Key::Str(name) = name else {
                continue;
            };
            #[allow(clippy::question_mark)]
            let Some(prop) = own_prop_atom(&self.ctx, options.as_raw(), key.atom) else {
                return None;
            };
            let Prop::Data(value) = prop else {
                return None;
            };
            let number = if let Some(int) = value.as_int() {
                Some(int as f64)
            } else {
                value.as_float()
            };
            let accepted = if units_eq_ascii(&name, "depth") {
                match number {
                    Some(value)
                        if value.is_finite()
                            && value.fract() == 0.0
                            && (-1.0..=1000.0).contains(&value) =>
                    {
                        depth = value as i64;
                        true
                    }
                    _ => false,
                }
            } else if units_eq_ascii(&name, "compact") {
                number == Some(COMPACT as f64)
            } else if units_eq_ascii(&name, "breakLength") {
                number == Some(BREAK_LENGTH as f64)
            } else if units_eq_ascii(&name, "showHidden")
                || units_eq_ascii(&name, "sorted")
                || units_eq_ascii(&name, "getters")
                || units_eq_ascii(&name, "colors")
            {
                // Falsy values (false / undefined) behave like the default.
                value.is_undefined() || value.as_bool() == Some(false)
            } else {
                false
            };
            if !accepted {
                return None;
            }
        }
        Some(depth)
    }

    fn render(&mut self, value: &Value<'js>, rt: i64) -> Option<String> {
        if value.is_undefined() {
            return Some("undefined".to_owned());
        }
        if value.is_null() {
            return Some("null".to_owned());
        }
        if value.is_bool() {
            return Some(if value.as_bool()? {
                "true".to_owned()
            } else {
                "false".to_owned()
            });
        }
        if value.is_number() {
            return number_text(&self.ctx, value);
        }
        if value.is_big_int() {
            let text = scalar_text(&self.ctx, value)?;
            return Some(format!("{text}n"));
        }
        if value.is_string() {
            let units = read_units(&self.ctx, value)?;
            return self.render_string(&units);
        }
        if value.is_symbol() {
            return symbol_text(&self.ctx, value);
        }
        if !value.is_object() || value.is_function() {
            return None;
        }
        if value.is_proxy() {
            return None;
        }
        // Custom `util.inspect.custom` (own or inherited) always delegates.
        if self.has_custom(value)? {
            return None;
        }
        match class_id(value) {
            CLASS_ARRAY => self.render_array(value, rt),
            CLASS_MAP => self.render_map(value, rt),
            CLASS_SET => self.render_set(value, rt),
            CLASS_DATE => self.render_date(value),
            CLASS_REGEXP => self.render_regexp(value),
            CLASS_OBJECT => self.render_object(value, rt),
            _ => None,
        }
    }

    fn render_string(&self, units: &[u16]) -> Option<String> {
        if units.len() > MAX_STRING_LENGTH {
            return None;
        }
        let long = units.len() > 16 && (units.len() as i64) > BREAK_LENGTH - self.indent as i64 - 4;
        if !long {
            return Some(quote_units(units));
        }
        // Node splits on newlines (keeping the newline in the preceding line).
        let mut parts: Vec<&[u16]> = Vec::new();
        let mut start = 0;
        for (index, &code) in units.iter().enumerate() {
            if code == 0x0A {
                parts.push(&units[start..=index]);
                start = index + 1;
            }
        }
        if start < units.len() {
            parts.push(&units[start..]);
        }
        let separator = format!(" +\n{}", " ".repeat(self.indent + 2));
        let mut out = String::new();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                out.push_str(&separator);
            }
            out.push_str(&quote_units(part));
        }
        Some(out)
    }

    fn render_key(&self, key: &Key) -> String {
        match key {
            Key::Str(units) => {
                if is_identifier(units) {
                    if units_eq_ascii(units, "__proto__") {
                        "['__proto__']".to_owned()
                    } else {
                        String::from_utf16(units).unwrap_or_default()
                    }
                } else {
                    quote_units(units)
                }
            }
            Key::Sym(description) => {
                let mut whole = Vec::with_capacity(description.len() + 8);
                whole.extend("Symbol(".encode_utf16());
                whole.extend_from_slice(description);
                whole.push(')' as u16);
                escape_no_quotes(&whole)
            }
        }
    }

    fn has_custom(&self, value: &Value<'js>) -> Option<bool> {
        let object = value.as_object()?;
        match object.contains_key(self.custom.clone()) {
            Ok(found) => Some(found),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    fn has_tag(&self, value: &Value<'js>) -> Option<bool> {
        let object = value.as_object()?;
        match object.contains_key(PredefinedAtom::SymbolToStringTag) {
            Ok(found) => Some(found),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    /// The string value of `value[Symbol.toStringTag]`, inherited or own.
    ///
    /// `Some(None)` means "no tag" in Node's sense: the property is absent or
    /// its value is not a string (Node coerces both to `tag = ''`). The outer
    /// `None` means the lookup itself failed (throwing getter), so the caller
    /// must delegate to the JS renderer.
    fn tag_text(&self, value: &Value<'js>) -> Option<Option<String>> {
        let object = value.as_object()?;
        let tag = match object.get::<_, Value>(PredefinedAtom::SymbolToStringTag) {
            Ok(tag) => tag,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if !tag.is_string() {
            return Some(None);
        }
        let text = read_string_fast(&self.ctx, &tag)?;
        Some(Some(text))
    }

    fn constructor_name(&self, first_proto: &Value<'js>) -> Option<String> {
        let mut current = first_proto.clone();
        loop {
            match own_prop_atom(
                &self.ctx,
                current.as_raw(),
                PredefinedAtom::Constructor as qjs::JSAtom,
            )? {
                Prop::Absent => {}
                Prop::Accessor => return None,
                Prop::Data(constructor) => {
                    if constructor.is_function() {
                        let name = match own_prop_atom(
                            &self.ctx,
                            constructor.as_raw(),
                            PredefinedAtom::Name as qjs::JSAtom,
                        ) {
                            Some(Prop::Data(raw_name)) => {
                                let units = if raw_name.is_string() {
                                    read_units(&self.ctx, &raw_name)
                                } else {
                                    None
                                };
                                units.and_then(|units| String::from_utf16(&units).ok())
                            }
                            // Accessor / missing `name`: nothing to report.
                            _ => None,
                        };
                        if let Some(name) = name.filter(|name| !name.is_empty()) {
                            let candidate = match own_prop_atom(
                                &self.ctx,
                                constructor.as_raw(),
                                PredefinedAtom::Prototype as qjs::JSAtom,
                            )? {
                                Prop::Data(prototype) => prototype == current,
                                _ => false,
                            };
                            if candidate {
                                return Some(name);
                            }
                        }
                    }
                }
            }
            #[allow(clippy::question_mark)]
            let Some(proto) = prototype_of(&self.ctx, &current)?
            else {
                return None;
            };
            current = proto;
        }
    }

    fn render_object(&mut self, value: &Value<'js>, rt: i64) -> Option<String> {
        let raw = value.as_raw();
        let proto = prototype_of(&self.ctx, value)?;

        // `Symbol.toStringTag` anywhere in the chain changes Node's prefix
        // (`Map [tag] {`); delegate rather than replicate that.
        if self.has_tag(value)? {
            return None;
        }

        let style = match &proto {
            None => "[Object: null prototype]".to_owned(),
            Some(proto) if *proto == self.object_proto => "Object".to_owned(),
            Some(proto) => {
                let name = self.constructor_name(proto)?;
                if name == "URL" || name == "URLSearchParams" {
                    return None;
                }
                name
            }
        };

        let keys = enum_keys(&self.ctx, raw, true)?;
        if keys.is_empty() {
            return Some(if style == "Object" {
                "{}".to_owned()
            } else {
                format!("{style} {{}}")
            });
        }

        if rt > self.depth {
            return Some(if style == "Object" {
                "[Object]".to_owned()
            } else if style == "[Object: null prototype]" {
                "[Object: null prototype]".to_owned()
            } else {
                format!("[{style}]")
            });
        }

        let crt = rt + 1;
        if crt > MAX_RENDER_DEPTH {
            return None;
        }
        if !self.push_seen(value) {
            return None;
        }
        self.last_rt = crt;
        let result = (|| {
            let mut entries = Vec::with_capacity(keys.len());
            for key in &keys {
                let prop = own_prop_atom(&self.ctx, raw, key.atom)?;
                let Prop::Data(entry_value) = prop else {
                    return None;
                };
                let key = atom_to_key(&self.ctx, key.atom)?;
                let name = self.render_key(&key);
                self.indent += 2;
                let rendered = self.render(&entry_value, crt);
                self.indent -= 2;
                entries.push(format!("{name}: {}", rendered?));
            }
            let brace = if style == "Object" {
                "{".to_owned()
            } else {
                format!("{style} {{")
            };
            Some(self.reduce(entries, "", &brace, "}", false, false, crt))
        })();
        self.seen.pop();
        result
    }

    fn render_array(&mut self, value: &Value<'js>, rt: i64) -> Option<String> {
        let raw = value.as_raw();
        let array_proto = class_proto(&self.ctx, CLASS_ARRAY)?;
        let proto = prototype_of(&self.ctx, value)?;
        if proto.as_ref() != Some(&array_proto) {
            return None;
        }
        if self.has_tag(value)? {
            return None;
        }

        let length_value = match value.as_object()?.get::<_, Value>(PredefinedAtom::Length) {
            Ok(value) => value,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        let length = to_i64(&self.ctx, &length_value)?;
        if length < 0 || length as usize > MAX_ARRAY_LENGTH {
            return None;
        }
        let length = length as usize;

        let keys = enum_keys(&self.ctx, raw, true)?;
        if keys.len() != length {
            // Sparse arrays, extra properties or symbol keys: delegate.
            return None;
        }

        let crt = rt + 1;
        if length == 0 {
            return Some("[]".to_owned());
        }
        if rt > self.depth {
            return Some("[Array]".to_owned());
        }
        if crt > MAX_RENDER_DEPTH {
            return None;
        }
        if !self.push_seen(value) {
            return None;
        }
        self.last_rt = crt;
        let result = (|| {
            let mut entries = Vec::with_capacity(length);
            let mut all_numeric = true;
            for index in 0..length {
                let prop = own_prop_index(&self.ctx, raw, index as u32)?;
                let Prop::Data(entry_value) = prop else {
                    return None;
                };
                all_numeric = all_numeric
                    && (entry_value.is_int() || entry_value.is_float() || entry_value.is_big_int());
                self.indent += 2;
                let rendered = self.render(&entry_value, crt);
                self.indent -= 2;
                entries.push(rendered?);
            }
            Some(self.reduce(entries, "", "[", "]", true, all_numeric, crt))
        })();
        self.seen.pop();
        result
    }

    fn read_size(&self, value: &Value<'js>) -> Option<i64> {
        let size_value = match value.as_object()?.get::<_, Value>("size") {
            Ok(value) => value,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        to_i64(&self.ctx, &size_value)
    }

    fn collection_keys_empty(&self, value: &Value<'js>) -> Option<bool> {
        let keys = enum_keys(&self.ctx, value.as_raw(), true)?;
        Some(keys.is_empty())
    }

    fn get_iterator(&self, value: &Value<'js>) -> Option<Value<'js>> {
        let iterator = match value
            .as_object()?
            .get::<_, Value>(PredefinedAtom::SymbolIterator)
        {
            Ok(iterator) => iterator,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if !iterator.is_function() {
            return None;
        }
        call_method(&self.ctx, &iterator, value.as_raw())
    }

    fn iterator_next(
        &self,
        iterator: &Value<'js>,
        next: &Value<'js>,
    ) -> Option<(bool, Value<'js>)> {
        let result = call_method(&self.ctx, next, iterator.as_raw())?;
        let result = result.as_object()?;
        let done = match result.get::<_, Value>(PredefinedAtom::Done) {
            Ok(done) => done,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        let done = match done.get::<Coerced<bool>>() {
            Ok(Coerced(done)) => done,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if done {
            return Some((true, Value::new_undefined(self.ctx.clone())));
        }
        match result.get::<_, Value>(PredefinedAtom::Value) {
            Ok(value) => Some((false, value)),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    fn render_map(&mut self, value: &Value<'js>, rt: i64) -> Option<String> {
        let map_proto = class_proto(&self.ctx, CLASS_MAP)?;
        let proto = prototype_of(&self.ctx, value)?;
        if proto.as_ref() != Some(&map_proto) {
            return None;
        }
        // Map.prototype carries the inherited `@@toStringTag = "Map"`; Node's
        // prefix is `Map(n) ` for exactly that value and `Map(n) [tag] ` for
        // anything else. Only the built-in tag keeps the native fast path.
        match self.tag_text(value)? {
            Some(ref tag) if tag == "Map" => {}
            _ => return None,
        }
        let size = self.read_size(value)?;
        if !self.collection_keys_empty(value)? {
            return None;
        }
        if size == 0 {
            return Some("Map(0) {}".to_owned());
        }
        if rt > self.depth {
            return Some("[Map]".to_owned());
        }
        let crt = rt + 1;
        if crt > MAX_RENDER_DEPTH {
            return None;
        }
        let limit = (size as usize).min(MAX_ARRAY_LENGTH);
        let remaining = size - limit as i64;

        let iterator = self.get_iterator(value)?;
        let next = match iterator.as_object()?.get::<_, Value>(PredefinedAtom::Next) {
            Ok(next) => next,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if !next.is_function() {
            return None;
        }
        if !self.push_seen(value) {
            return None;
        }
        self.last_rt = crt;
        let result = (|| {
            let mut entries = Vec::with_capacity(limit + usize::from(remaining > 0));
            self.indent += 2;
            for _ in 0..limit {
                let (done, entry) = self.iterator_next(&iterator, &next)?;
                if done {
                    break;
                }
                let key = match entry.as_object()?.get::<_, Value>(0u32) {
                    Ok(key) => key,
                    Err(_) => {
                        clear_error(&self.ctx);
                        self.indent -= 2;
                        return None;
                    }
                };
                let entry_value = match entry.as_object()?.get::<_, Value>(1u32) {
                    Ok(value) => value,
                    Err(_) => {
                        clear_error(&self.ctx);
                        self.indent -= 2;
                        return None;
                    }
                };
                let rendered_key = self.render(&key, crt);
                let rendered_value = self.render(&entry_value, crt);
                match (rendered_key, rendered_value) {
                    (Some(key), Some(value)) => entries.push(format!("{key} => {value}")),
                    _ => {
                        self.indent -= 2;
                        return None;
                    }
                }
            }
            self.indent -= 2;
            if remaining > 0 {
                entries.push(format!(
                    "... {remaining} more item{}",
                    if remaining > 1 { "s" } else { "" }
                ));
            }
            let brace = format!("Map({size}) {{");
            Some(self.reduce(entries, "", &brace, "}", false, false, crt))
        })();
        self.seen.pop();
        result
    }

    fn render_set(&mut self, value: &Value<'js>, rt: i64) -> Option<String> {
        let set_proto = class_proto(&self.ctx, CLASS_SET)?;
        let proto = prototype_of(&self.ctx, value)?;
        if proto.as_ref() != Some(&set_proto) {
            return None;
        }
        // Same rule as render_map: only the built-in inherited tag is claimed.
        match self.tag_text(value)? {
            Some(ref tag) if tag == "Set" => {}
            _ => return None,
        }
        let size = self.read_size(value)?;
        if !self.collection_keys_empty(value)? {
            return None;
        }
        if size == 0 {
            return Some("Set(0) {}".to_owned());
        }
        if rt > self.depth {
            return Some("[Set]".to_owned());
        }
        let crt = rt + 1;
        if crt > MAX_RENDER_DEPTH {
            return None;
        }
        let limit = (size as usize).min(MAX_ARRAY_LENGTH);
        let remaining = size - limit as i64;

        let iterator = self.get_iterator(value)?;
        let next = match iterator.as_object()?.get::<_, Value>(PredefinedAtom::Next) {
            Ok(next) => next,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if !next.is_function() {
            return None;
        }
        if !self.push_seen(value) {
            return None;
        }
        self.last_rt = crt;
        let result = (|| {
            let mut entries = Vec::with_capacity(limit + usize::from(remaining > 0));
            self.indent += 2;
            for _ in 0..limit {
                let (done, entry) = self.iterator_next(&iterator, &next)?;
                if done {
                    break;
                }
                let rendered = self.render(&entry, crt);
                match rendered {
                    Some(rendered) => entries.push(rendered),
                    None => {
                        self.indent -= 2;
                        return None;
                    }
                }
            }
            self.indent -= 2;
            if remaining > 0 {
                entries.push(format!(
                    "... {remaining} more item{}",
                    if remaining > 1 { "s" } else { "" }
                ));
            }
            let brace = format!("Set({size}) {{");
            Some(self.reduce(entries, "", &brace, "}", false, false, crt))
        })();
        self.seen.pop();
        result
    }

    fn render_date(&mut self, value: &Value<'js>) -> Option<String> {
        let raw = value.as_raw();
        let date_proto = class_proto(&self.ctx, CLASS_DATE)?;
        let proto = prototype_of(&self.ctx, value)?;
        if proto.as_ref() != Some(&date_proto) {
            return None;
        }
        // Date.prototype has no `@@toStringTag` by default; any tag (own or
        // inherited) changes Node's prefix to `Date [tag] ...`.
        if self.has_tag(value)? || !self.collection_keys_empty(value)? {
            return None;
        }
        // Node's Date branch calls `value.getTime()` / `value.toISOString()`
        // (and `String(value)` for invalid dates); an own shadow changes that.
        for name in ["getTime", "toISOString", "toString"] {
            if !matches!(own_prop_named(&self.ctx, raw, name)?, Prop::Absent) {
                return None;
            }
        }
        let get_time = match date_proto.as_object()?.get::<_, Value>("getTime") {
            Ok(get_time) => get_time,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        let result = call_method(&self.ctx, &get_time, raw)?;
        let millis = match result.get::<Coerced<f64>>() {
            Ok(Coerced(millis)) => millis,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if millis.is_nan() {
            return Some("Invalid Date".to_owned());
        }
        date_to_iso(millis)
    }

    fn render_regexp(&mut self, value: &Value<'js>) -> Option<String> {
        let raw = value.as_raw();
        let regexp_proto = class_proto(&self.ctx, CLASS_REGEXP)?;
        let proto = prototype_of(&self.ctx, value)?;
        if proto.as_ref() != Some(&regexp_proto) {
            return None;
        }
        // RegExp.prototype has no `@@toStringTag` by default; any tag (own or
        // inherited) adds Node's `RegExp [tag] ` prefix.
        if self.has_tag(value)? || !self.collection_keys_empty(value)? {
            return None;
        }
        // An own `toString` would shadow `String(regexp)`.
        if !matches!(
            own_prop_atom(&self.ctx, raw, PredefinedAtom::ToString as qjs::JSAtom)?,
            Prop::Absent
        ) {
            return None;
        }
        // Node renders `String(value)`; `RegExp.prototype.toString` is a data
        // method, so read it from the (live) built-in prototype and call it
        // with the instance as `this`.
        let to_string = match regexp_proto
            .as_object()?
            .get::<_, Value>(PredefinedAtom::ToString)
        {
            Ok(to_string) => to_string,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if !to_string.is_function() {
            return None;
        }
        let rendered = call_method(&self.ctx, &to_string, raw)?;
        let units = read_units(&self.ctx, &rendered)?;
        String::from_utf16(&units).ok()
    }

    fn push_seen(&mut self, value: &Value<'js>) -> bool {
        if self.seen.contains(value) {
            return false;
        }
        self.seen.push(value.clone());
        true
    }

    fn is_below_break(&self, output: &[String], start: usize, base: &str) -> bool {
        let break_length = BREAK_LENGTH as usize;
        let mut total = output.len() + start;
        if total + output.len() > break_length {
            return false;
        }
        for entry in output {
            total += width(entry);
            if total > break_length {
                return false;
            }
        }
        !base.contains('\n')
    }

    fn group_array_elements(&self, output: Vec<String>, numeric: bool) -> Vec<String> {
        let output_length = output.len();
        if output_length == 0 {
            return output;
        }
        let mut data_len = Vec::with_capacity(output_length);
        let mut total_length: f64 = 0.0;
        let mut max_length: i64 = 0;
        for entry in &output {
            let len = width(entry) as i64;
            data_len.push(len);
            total_length += (len + 2) as f64;
            if len > max_length {
                max_length = len;
            }
        }
        let actual_max = max_length + 2;
        let indent = self.indent as f64;
        let break_length = BREAK_LENGTH as f64;
        if !((actual_max as f64) * 3.0 + indent < break_length
            && (total_length / actual_max as f64 > 5.0 || max_length <= 6))
        {
            return output;
        }
        let approx = 2.5f64;
        let average_bias = (actual_max as f64 - total_length / output_length as f64).sqrt();
        let biased_max = ((actual_max - 3) as f64 - average_bias).max(1.0);
        let columns = ((approx * biased_max * output_length as f64).sqrt() / biased_max)
            .round()
            .min(((break_length - indent) / actual_max as f64).floor())
            .min((COMPACT * 4) as f64)
            .min(15.0);
        let columns = columns as i64;
        if columns <= 1 {
            return output;
        }

        let mut max_line_length = vec![0i64; columns as usize];
        let stride = columns as usize;
        for (column, max_len) in max_line_length.iter_mut().enumerate() {
            let mut line_max = 0i64;
            let mut index = column;
            while index < output.len() {
                if data_len[index] > line_max {
                    line_max = data_len[index];
                }
                index += stride;
            }
            *max_len = line_max + 2;
        }

        let mut result = Vec::new();
        let mut index = 0usize;
        while index < output_length {
            let end = (index + columns as usize).min(output_length);
            let mut line = String::new();
            let mut offset = index;
            while offset + 1 < end {
                let target = (max_line_length[offset - index] + width(&output[offset]) as i64
                    - data_len[offset]) as usize;
                line.push_str(&pad(&format!("{}, ", output[offset]), target, numeric));
                offset += 1;
            }
            if numeric {
                let target = (max_line_length[offset - index] + width(&output[offset]) as i64
                    - data_len[offset]
                    - 2) as usize;
                line.push_str(&pad(&output[offset], target, true));
            } else {
                line.push_str(&output[offset]);
            }
            result.push(line);
            index += columns as usize;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn reduce(
        &self,
        mut output: Vec<String>,
        base: &str,
        brace_open: &str,
        brace_close: &str,
        array_extras: bool,
        numeric: bool,
        rt: i64,
    ) -> String {
        let original_len = output.len();
        if array_extras && output.len() > 6 {
            output = self.group_array_elements(output, numeric);
        }
        if self.last_rt - rt < COMPACT && output.len() == original_len {
            let start = output.len() + self.indent + width(brace_open) + width(base) + 10;
            if self.is_below_break(&output, start, base) {
                let joined = output.join(", ");
                if !joined.contains('\n') {
                    return if base.is_empty() {
                        format!("{brace_open} {joined} {brace_close}")
                    } else {
                        format!("{base} {brace_open} {joined} {brace_close}")
                    };
                }
            }
        }
        let indentation = " ".repeat(self.indent);
        let body = output.join(&format!(",\n{indentation}  "));
        if base.is_empty() {
            format!("{brace_open}\n{indentation}  {body}\n{indentation}{brace_close}")
        } else {
            format!("{base} {brace_open}\n{indentation}  {body}\n{indentation}{brace_close}")
        }
    }
}

/// Native `util.format` fast path.
///
/// It mirrors `pirFormatWithOptionsInternal` for the default option set and
/// answers the JS value `undefined` whenever the format string (or an
/// argument's rendering) is outside the fast path, so the faithful JS
/// implementation remains the fallback.
struct Formatter<'js> {
    ctx: Ctx<'js>,
    args: rquickjs::Array<'js>,
    args_len: usize,
    inspect: Option<Renderer<'js>>,
    global: Option<Object<'js>>,
}

impl<'js> Formatter<'js> {
    fn new(ctx: Ctx<'js>, args: &Value<'js>) -> Option<Self> {
        let array = args.as_array()?.clone();
        let length_value = match args.as_object()?.get::<_, Value>(PredefinedAtom::Length) {
            Ok(value) => value,
            Err(_) => {
                clear_error(&ctx);
                return None;
            }
        };
        let length_value = to_i64(&ctx, &length_value)?;
        if length_value < 0 || length_value > u32::MAX as i64 {
            return None;
        }
        Some(Formatter {
            ctx,
            args: array,
            args_len: length_value as usize,
            inspect: None,
            global: None,
        })
    }

    fn arg(&mut self, index: usize) -> Option<Value<'js>> {
        if index >= self.args_len {
            return None;
        }
        match self.args.get::<Value>(index) {
            Ok(value) => Some(value),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    fn global(&mut self) -> Object<'js> {
        if self.global.is_none() {
            self.global = Some(self.ctx.globals());
        }
        self.global
            .as_ref()
            .unwrap_or_else(|| panic!("global object not initialized"))
            .clone()
    }

    fn global_value(&mut self, name: &str) -> Option<Value<'js>> {
        let global = self.global();
        match global.get::<_, Value>(name) {
            Ok(value) => Some(value),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    fn call_global_1(&mut self, name: &str, value: &Value<'js>) -> Option<Value<'js>> {
        let function = self.global_value(name)?;
        let function = function.into_function()?;
        match function.call::<_, Value>((value.clone(),)) {
            Ok(result) => Some(result),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    fn json_stringify(&mut self, value: &Value<'js>) -> Option<String> {
        let json = self.global_value("JSON")?;
        let json = json.as_object()?;
        let stringify = match json.get::<_, Value>("stringify") {
            Ok(stringify) => stringify,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        let stringify = stringify.into_function()?;
        let result = match stringify.call::<_, Value>((value.clone(),)) {
            Ok(result) => result,
            Err(_) => {
                // Circular structures (and every other throw) go back through
                // the faithful JS path, which produces `[Circular]` or
                // rethrows.
                clear_error(&self.ctx);
                return None;
            }
        };
        if result.is_undefined() {
            return Some("undefined".to_owned());
        }
        if result.is_string() {
            return read_string_fast(&self.ctx, &result);
        }
        // A tampered `JSON.stringify` result is coerced by string
        // concatenation.
        match result.get::<Coerced<String>>() {
            Ok(Coerced(text)) => Some(text),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    fn inspect_value(&mut self, value: &Value<'js>, depth: i64) -> Option<String> {
        if self.inspect.is_none() {
            self.inspect = Some(Renderer::new(self.ctx.clone())?);
        }
        let renderer = self.inspect.as_mut()?;
        renderer.depth = depth;
        renderer.indent = 0;
        renderer.last_rt = 0;
        renderer.seen.clear();
        renderer.render(value, 0)
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_string_value(&mut self, value: &Value<'js>) -> Option<String> {
        if value.is_string() {
            return read_string_fast(&self.ctx, value);
        }
        if value.is_symbol() {
            return symbol_text(&self.ctx, value);
        }
        match value.get::<Coerced<String>>() {
            Ok(Coerced(text)) => Some(text),
            Err(_) => {
                clear_error(&self.ctx);
                None
            }
        }
    }

    /// Mirror `pirHasBuiltInToString` conservatively: answer `true` only when
    /// `%s` takes the `inspect(..., {depth: 0})` branch, otherwise delegate.
    fn string_arg_uses_inspect(&mut self, value: &Value<'js>) -> Option<bool> {
        if value.is_proxy() {
            return None;
        }
        let raw = value.as_raw();
        // An own `toString`/`Symbol.toPrimitive` (data or accessor) changes the
        // decision in ways that depend on user code; delegate.
        if !matches!(own_prop_named(&self.ctx, raw, "toString")?, Prop::Absent) {
            return None;
        }
        if !matches!(
            own_prop_atom(
                &self.ctx,
                raw,
                PredefinedAtom::SymbolToPrimitive as qjs::JSAtom
            )?,
            Prop::Absent
        ) {
            return None;
        }

        let mut pointer = prototype_of(&self.ctx, value)?;
        while let Some(current) = pointer {
            if current.is_proxy() {
                return None;
            }
            let to_string = own_prop_named(&self.ctx, current.as_raw(), "toString")?;
            let to_primitive = own_prop_atom(
                &self.ctx,
                current.as_raw(),
                PredefinedAtom::SymbolToPrimitive as qjs::JSAtom,
            )?;
            let owns_to_string = !matches!(to_string, Prop::Absent);
            let owns_to_primitive = !matches!(to_primitive, Prop::Absent);
            if owns_to_string || owns_to_primitive {
                if matches!(to_string, Prop::Accessor) || matches!(to_primitive, Prop::Accessor) {
                    return None;
                }
                let name = own_constructor_name(&self.ctx, &current)?;
                return if is_std_constructor(&name) {
                    Some(true)
                } else {
                    None
                };
            }
            pointer = prototype_of(&self.ctx, &current)?;
        }
        Some(true)
    }

    fn format_string_arg(&mut self, value: &Value<'js>) -> Option<String> {
        if value.is_number() {
            return number_text(&self.ctx, value);
        }
        if value.is_big_int() {
            return Some(format!("{}n", scalar_text(&self.ctx, value)?));
        }
        // `typeof value !== "object"` in the shim: functions take this branch.
        if !value.is_object() || value.is_function() {
            return self.to_string_value(value);
        }
        match self.string_arg_uses_inspect(value) {
            Some(true) => self.inspect_value(value, 0),
            _ => None,
        }
    }

    fn format_number(&mut self, value: &Value<'js>) -> Option<String> {
        if value.is_number() {
            return number_text(&self.ctx, value);
        }
        let number = match value.get::<Coerced<f64>>() {
            Ok(Coerced(number)) => number,
            Err(_) => {
                clear_error(&self.ctx);
                return None;
            }
        };
        if number == 0.0 {
            return Some(if number.is_sign_negative() {
                "-0".to_owned()
            } else {
                "0".to_owned()
            });
        }
        let value = Value::new_float(self.ctx.clone(), number);
        scalar_text(&self.ctx, &value)
    }

    fn format_global_number(&mut self, name: &str, value: &Value<'js>) -> Option<String> {
        let result = self.call_global_1(name, value)?;
        if !result.is_number() {
            return None;
        }
        number_text(&self.ctx, &result)
    }

    fn push_bigint(&mut self, value: &Value<'js>) -> Option<String> {
        Some(format!("{}n", scalar_text(&self.ctx, value)?))
    }

    fn format(&mut self, fmt: &Value<'js>) -> Option<String> {
        if !fmt.is_string() {
            return None;
        }
        let format = read_string_fast(&self.ctx, fmt)?;
        if self.args_len == 1 {
            return Some(format);
        }
        let bytes = format.as_bytes();
        let args_len = self.args_len;
        let mut a = 0usize;
        let mut out = String::new();
        let mut join = "";
        let mut last_pos = 0usize;
        let mut i = 0usize;

        while i + 1 < bytes.len() {
            if bytes[i] != b'%' {
                i += 1;
                continue;
            }
            i += 1;
            let specifier = bytes[i];
            if a + 1 != args_len {
                let rendered = match specifier {
                    b's' => {
                        a += 1;
                        let value = self.arg(a)?;
                        self.format_string_arg(&value)?
                    }
                    b'j' => {
                        a += 1;
                        let value = self.arg(a)?;
                        self.json_stringify(&value)?
                    }
                    b'd' => {
                        a += 1;
                        let value = self.arg(a)?;
                        if value.is_big_int() {
                            self.push_bigint(&value)?
                        } else if value.is_symbol() {
                            "NaN".to_owned()
                        } else {
                            self.format_number(&value)?
                        }
                    }
                    b'O' => {
                        a += 1;
                        let value = self.arg(a)?;
                        self.inspect_value(&value, 2)?
                    }
                    // `%o` asks inspect for `{showHidden, showProxy, depth: 4}`,
                    // which the native renderer does not model.
                    b'o' => return None,
                    b'i' => {
                        a += 1;
                        let value = self.arg(a)?;
                        if value.is_big_int() {
                            self.push_bigint(&value)?
                        } else if value.is_symbol() {
                            "NaN".to_owned()
                        } else {
                            self.format_global_number("parseInt", &value)?
                        }
                    }
                    b'f' => {
                        a += 1;
                        let value = self.arg(a)?;
                        if value.is_symbol() {
                            "NaN".to_owned()
                        } else {
                            self.format_global_number("parseFloat", &value)?
                        }
                    }
                    b'c' => {
                        a += 1;
                        String::new()
                    }
                    b'%' => {
                        if last_pos < i {
                            out.push_str(&format[last_pos..i]);
                        }
                        last_pos = i + 1;
                        i += 1;
                        continue;
                    }
                    _ => {
                        // Not a placeholder: keep the `%` and the character.
                        i += 1;
                        continue;
                    }
                };
                if last_pos < i - 1 {
                    out.push_str(&format[last_pos..i - 1]);
                }
                out.push_str(&rendered);
                last_pos = i + 1;
            } else if specifier == b'%' {
                if last_pos < i {
                    out.push_str(&format[last_pos..i]);
                }
                last_pos = i + 1;
            }
            i += 1;
        }

        if last_pos != 0 {
            a += 1;
            join = " ";
            if last_pos < format.len() {
                out.push_str(&format[last_pos..]);
            }
        }
        while a < args_len {
            let value = self.arg(a)?;
            out.push_str(join);
            if value.is_string() {
                out.push_str(&read_string_fast(&self.ctx, &value)?);
            } else {
                out.push_str(&self.inspect_value(&value, 2)?);
            }
            join = " ";
            a += 1;
        }
        Some(out)
    }
}

/// `__pirFormat(fmt, args)` — returns the formatted string, or the JS value
/// `undefined` when the call is outside the fast path.
pub(super) fn format_native<'js>(
    ctx: Ctx<'js>,
    fmt: Value<'js>,
    args: Value<'js>,
) -> Result<Value<'js>> {
    let Some(mut formatter) = Formatter::new(ctx.clone(), &args) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(rendered) = formatter.format(&fmt) else {
        return Ok(Value::new_undefined(ctx));
    };
    rendered.into_js(&ctx)
}

/// `__pirInspect(value[, options])` — returns the rendered string, or
/// `undefined` when the value (or the option set) is outside the fast path.
pub(super) fn inspect_native<'js>(
    ctx: Ctx<'js>,
    value: Value<'js>,
    options: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some(mut renderer) = Renderer::new(ctx.clone()) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(depth) = renderer.parse_options(options.0.as_ref()) else {
        return Ok(Value::new_undefined(ctx));
    };
    renderer.depth = depth;
    match renderer.render(&value, 0) {
        Some(rendered) => rendered.into_js(&ctx),
        None => Ok(Value::new_undefined(ctx)),
    }
}

/// Install the inspect fast-path global.
pub(super) fn register_inspect_ops(ctx: &Ctx<'_>) -> Result<()> {
    ctx.globals().set(
        "__pirInspect",
        rquickjs::prelude::Func::from(inspect_native),
    )?;
    ctx.globals()
        .set("__pirFormat", rquickjs::prelude::Func::from(format_native))
}
