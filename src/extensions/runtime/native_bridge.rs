//! Native (rquickjs C-ABI) value bridge — the JSON-free counterpart to
//! `host_functions`'s `__pir_host` string envelope.
//!
//! The `pi`/`ctx` API is JSON-*shaped*, but the shape does not have to be
//! encoded as a JSON string to cross the host boundary. This module converts
//! `serde_json::Value` trees to live QuickJS objects/arrays and back using
//! only rquickjs's typed `Object`/`Array`/`Value` API — no `to_string`/`from_str`
//! at the seam. This is the same tier as `host_functions::buffer_ops`: the
//! prelude keeps `__pir_host` string calls as a fallback so a boot path without
//! `__pir_native` still works.

use rquickjs::{Array, Ctx, Error as JsError, Object, Result as JsResult, Type, Value};
use serde_json::{Map, Number, Value as JsonValue};

const EXPECTED_OBJECT: &str = "object";

/// Depth cap for the recursive JS→JSON walk. Without it a plugin can build a
/// pathologically nested value and abort the process with a native stack
/// overflow (uncatchable from JS). The cap turns that into a JS error instead,
/// mirroring what `JSON.stringify` does for the same input.
const JS_TO_JSON_MAX_DEPTH: usize = 4096;

fn js_err(kind: &'static str) -> JsError {
    JsError::new_into_js(kind, EXPECTED_OBJECT)
}

/// Set an own data property even when the key is `__proto__`.
///
/// `Object::set` goes through the ordinary `[[Set]]` path, so a key spelled
/// `__proto__` hits `Object.prototype.__proto__`'s setter: the value is lost,
/// or the brand-new object's prototype is re-pointed. Host JSON can legally
/// carry that key (a SQLite column alias, for instance), so the rare case is
/// routed through `Object.defineProperty`, which always creates a data
/// property. Every other key keeps the direct setter fast path.
fn set_own<'js, V: rquickjs::IntoJs<'js>>(
    obj: &Object<'js>,
    key: &str,
    value: V,
) -> JsResult<()> {
    if key != "__proto__" {
        return obj.set(key, value);
    }
    let object_ctor: Object = obj.ctx().globals().get("Object")?;
    let define: rquickjs::Function = object_ctor.get("defineProperty")?;
    let descriptor = Object::new(obj.ctx().clone())?;
    descriptor.set("value", value)?;
    descriptor.set("writable", true)?;
    descriptor.set("enumerable", true)?;
    descriptor.set("configurable", true)?;
    define.call::<_, ()>((obj.clone(), key, descriptor))
}

fn number_from_f64(f: f64) -> JsonValue {
    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        JsonValue::Number(Number::from(f as i64))
    } else {
        Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null)
    }
}

// ---------------------------------------------------------------------------
// JSON -> JS (iterative; `session_tree` can be very deep)
// ---------------------------------------------------------------------------

fn json_leaf<'js>(ctx: &Ctx<'js>, v: &JsonValue) -> JsResult<Value<'js>> {
    Ok(match v {
        JsonValue::Null => Value::new_null(ctx.clone()),
        JsonValue::Bool(b) => Value::new_bool(ctx.clone(), *b),
        JsonValue::Number(n) => {
            let f = n.as_f64().ok_or_else(|| js_err("finite JSON number"))?;
            Value::new_number(ctx.clone(), f)
        }
        JsonValue::String(s) => rquickjs::String::from_str(ctx.clone(), s.as_str())?.into_value(),
        JsonValue::Object(_) | JsonValue::Array(_) => return Err(js_err("JSON leaf")),
    })
}

/// Iterator over one object frame's `(key, value)` entries.
///
/// The borrowing walk (`make_child`) has to clone each entry into an owned
/// iterator; the owning walk (`make_child_owned`) moves serde_json's own map
/// iterator. One type keeps a single [`BuildFrame`] for both conversions
/// without cloning anything on the owning path.
enum ObjectIter {
    Cloned(std::vec::IntoIter<(String, JsonValue)>),
    Moved(serde_json::map::IntoIter),
}

impl Iterator for ObjectIter {
    type Item = (String, JsonValue);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ObjectIter::Cloned(iter) => iter.next(),
            ObjectIter::Moved(iter) => iter.next(),
        }
    }
}

enum BuildFrame<'js> {
    Object {
        obj: Object<'js>,
        iter: ObjectIter,
    },
    Array {
        arr: Array<'js>,
        iter: std::vec::IntoIter<JsonValue>,
        index: usize,
    },
}

enum Slot {
    Key(String),
    Index(usize),
}

fn install<'js, V: rquickjs::IntoJs<'js>>(
    frame: &mut BuildFrame<'js>,
    slot: Slot,
    value: V,
) -> JsResult<()> {
    match (frame, slot) {
        (BuildFrame::Object { obj, .. }, Slot::Key(k)) => set_own(obj, &k, value),
        (BuildFrame::Array { arr, index, .. }, Slot::Index(i)) => {
            *index = i + 1;
            arr.set(i, value)
        }
        _ => Err(js_err("JSON container slot")),
    }
}

fn make_child<'js>(
    ctx: &Ctx<'js>,
    v: &JsonValue,
) -> JsResult<(Value<'js>, Option<BuildFrame<'js>>)> {
    match v {
        JsonValue::Object(map) => {
            let obj = Object::new(ctx.clone())?;
            let iter = ObjectIter::Cloned(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>()
                    .into_iter(),
            );
            Ok((
                obj.clone().into_value(),
                Some(BuildFrame::Object { obj, iter }),
            ))
        }
        JsonValue::Array(list) => {
            let arr = Array::new(ctx.clone())?;
            Ok((
                arr.clone().into_value(),
                Some(BuildFrame::Array {
                    arr,
                    iter: list.clone().into_iter(),
                    index: 0,
                }),
            ))
        }
        leaf => Ok((json_leaf(ctx, leaf)?, None)),
    }
}

/// Convert a `serde_json::Value` into a live QuickJS value, without JSON text.
pub fn json_to_js<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> JsResult<Value<'js>> {
    let (root, frame) = make_child(ctx, value)?;
    let mut stack: Vec<BuildFrame<'js>> = frame.into_iter().collect();

    while let Some(top) = stack.last_mut() {
        let next = match top {
            BuildFrame::Object { iter, .. } => iter.next().map(|(k, v)| (Slot::Key(k), v)),
            BuildFrame::Array { iter, index, .. } => iter.next().map(|v| {
                let slot = Slot::Index(*index);
                (slot, v)
            }),
        };
        match next {
            None => {
                stack.pop();
            }
            Some((slot, child)) => {
                let (js_value, child_frame) = make_child(ctx, &child)?;
                install(stack.last_mut().unwrap(), slot, js_value)?;
                if let Some(frame) = child_frame {
                    stack.push(frame);
                }
            }
        }
    }
    Ok(root)
}

/// Owning counterpart of `make_child`: consumes the value's containers
/// instead of cloning every key and subtree into the frame iterators.
fn make_child_owned<'js>(
    ctx: &Ctx<'js>,
    value: JsonValue,
) -> JsResult<(Value<'js>, Option<BuildFrame<'js>>)> {
    match value {
        JsonValue::Object(map) => {
            let obj = Object::new(ctx.clone())?;
            Ok((
                obj.clone().into_value(),
                Some(BuildFrame::Object {
                    obj,
                    iter: ObjectIter::Moved(map.into_iter()),
                }),
            ))
        }
        JsonValue::Array(list) => {
            let arr = Array::new(ctx.clone())?;
            Ok((
                arr.clone().into_value(),
                Some(BuildFrame::Array {
                    arr,
                    iter: list.into_iter(),
                    index: 0,
                }),
            ))
        }
        leaf => Ok((json_leaf(ctx, &leaf)?, None)),
    }
}

/// Convert an owned `serde_json::Value` into a live QuickJS value without
/// JSON text and without a second deep copy.
///
/// [`json_to_js`] borrows its input, so it has to clone every container entry
/// while walking the tree. Callers that already own a privately built tree
/// (the native dispatch/tool-call seam) use this instead: the exact same
/// traversal, order and mapping, with the input's own buffers moved into the
/// frame iterators. Peak resident memory for one conversion is the tree, not
/// the tree twice.
pub fn json_to_js_owned<'js>(ctx: &Ctx<'js>, value: JsonValue) -> JsResult<Value<'js>> {
    let (root, frame) = make_child_owned(ctx, value)?;
    let mut stack: Vec<BuildFrame<'js>> = frame.into_iter().collect();

    while let Some(top) = stack.last_mut() {
        let next = match top {
            BuildFrame::Object { iter, .. } => iter.next().map(|(k, v)| (Slot::Key(k), v)),
            BuildFrame::Array { iter, index, .. } => iter.next().map(|v| {
                let slot = Slot::Index(*index);
                (slot, v)
            }),
        };
        match next {
            None => {
                stack.pop();
            }
            Some((slot, child)) => {
                let (js_value, child_frame) = make_child_owned(ctx, child)?;
                install(stack.last_mut().unwrap(), slot, js_value)?;
                if let Some(frame) = child_frame {
                    stack.push(frame);
                }
            }
        }
    }
    Ok(root)
}

// ---------------------------------------------------------------------------
// JS -> JSON (recursive; caller payloads are bounded by the engine's heap cap)
// ---------------------------------------------------------------------------

fn js_scalar(value: &Value<'_>) -> JsResult<JsonValue> {
    Ok(match value.type_of() {
        Type::Undefined | Type::Uninitialized | Type::Null => JsonValue::Null,
        Type::Bool => JsonValue::Bool(value.as_bool().unwrap_or(false)),
        Type::Int | Type::Float => number_from_f64(value.as_number().unwrap_or(0.0)),
        Type::BigInt => {
            let big: rquickjs::BigInt = value
                .clone()
                .into_big_int()
                .ok_or_else(|| js_err("bigint"))?;
            let i = big.to_i64()?;
            JsonValue::Number(Number::from(i))
        }
        Type::String => {
            let s: String = value
                .clone()
                .into_string()
                .ok_or_else(|| js_err("string"))?
                .to_string()?;
            JsonValue::String(s)
        }
        // Functions/renderers cannot cross the JSON shape; `JSON.stringify`
        // dropped them too.
        _ => JsonValue::Null,
    })
}

/// Convert a live QuickJS value tree into `serde_json::Value`, without JSON
/// text. Functions and symbols map to `null` (matching `JSON.stringify`).
pub fn js_to_json(value: &Value<'_>) -> JsResult<JsonValue> {
    js_to_json_at(value, 0)
}

fn js_to_json_at(value: &Value<'_>, depth: usize) -> JsResult<JsonValue> {
    if depth > JS_TO_JSON_MAX_DEPTH {
        let ctx = value.ctx();
        return Err(match rquickjs::Exception::from_message(
            ctx.clone(),
            "Maximum call stack size exceeded",
        ) {
            Ok(exception) => ctx.throw(exception.into_value()),
            Err(error) => error,
        });
    }
    Ok(match value.type_of() {
        Type::Array => {
            let arr = value.clone().into_array().ok_or_else(|| js_err("array"))?;
            let mut list = Vec::with_capacity(arr.len());
            for i in 0..arr.len() {
                list.push(js_to_json_at(&arr.get::<Value>(i)?, depth + 1)?);
            }
            JsonValue::Array(list)
        }
        Type::Object => {
            let obj = value
                .clone()
                .into_object()
                .ok_or_else(|| js_err("object"))?;
            let mut map = Map::new();
            for (key, child) in obj
                .own_props::<String, Value>(rquickjs::Filter::new().string().enum_only())
                .collect::<JsResult<Vec<_>>>()?
            {
                map.insert(key, js_to_json_at(&child, depth + 1)?);
            }
            JsonValue::Object(map)
        }
        _ => js_scalar(value)?,
    })
}

/// `globalThis.__pir_native` — installed by `register_host_functions` alongside
/// the `__pir_host` string table.
pub const NATIVE_GLOBAL: &str = "__pir_native";

