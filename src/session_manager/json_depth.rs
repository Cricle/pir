//! Stack-safe JSON text and teardown helpers for deep payloads.
//!
//! `serde_json` has no stack-safe path for a deeply nested [`JsonValue`]:
//! `to_string` / `to_vec` recurse one serializer frame per nesting level, and
//! the value's own drop glue recurses one frame per level as well (there is no
//! iterative teardown to opt into). A session tree nests one level per entry,
//! so a long linear session turns every one of those operations into a
//! session-length recursion — a ~1400-entry tree overran a 2 MiB thread stack.
//!
//! These helpers walk a value with an explicit worklist instead:
//!
//! - [`write_value_iteratively`] writes exactly what
//!   `serde_json::to_string` would (every leaf — string, number, bool, null —
//!   is encoded by `serde_json` itself, so only container punctuation is
//!   written here);
//! - [`drop_value_iteratively`] moves every container's children onto a
//!   worklist before the container itself is dropped;
//! - [`push_field`] appends one `"key":value` pair to a hand-assembled JSON
//!   object (the shape the session snapshot/event writers use to splice a
//!   pre-rendered tree fragment into a payload without ever holding the nested
//!   tree as a `Value`).

use serde::Serialize;
use serde_json::Value as JsonValue;

/// Compact JSON text of `value`, produced with an explicit worklist.
///
/// Byte-identical to `serde_json::to_string(value)` — the same leaf encoding
/// and, under `preserve_order`, the same key order — without the recursion.
pub fn write_value_iteratively(value: &JsonValue) -> String {
    enum Task<'a> {
        /// Emit one value (its container punctuation included).
        Value(&'a JsonValue),
        /// Emit a pre-encoded map key as `"key":`.
        Key(&'a str),
        /// Emit one structural character (`[`, `]`, `{`, `}`, `,`).
        Literal(&'static str),
    }
    let mut out = String::new();
    let mut stack: Vec<Task<'_>> = vec![Task::Value(value)];
    while let Some(task) = stack.pop() {
        match task {
            Task::Literal(text) => out.push_str(text),
            Task::Key(key) => {
                out.push_str(&leaf_text(key));
                out.push(':');
            }
            Task::Value(value) => match value {
                JsonValue::Null => out.push_str("null"),
                JsonValue::Bool(true) => out.push_str("true"),
                JsonValue::Bool(false) => out.push_str("false"),
                JsonValue::Number(number) => out.push_str(&number.to_string()),
                JsonValue::String(text) => out.push_str(&leaf_text(text)),
                JsonValue::Array(items) => {
                    if items.is_empty() {
                        out.push_str("[]");
                        continue;
                    }
                    out.push('[');
                    stack.push(Task::Literal("]"));
                    // Reversed pushes make `pop` emit element 0 first; each
                    // element's separator is pushed after it so it is emitted
                    // next (never before the element).
                    for index in (0..items.len()).rev() {
                        stack.push(Task::Value(&items[index]));
                        if index > 0 {
                            stack.push(Task::Literal(","));
                        }
                    }
                }
                JsonValue::Object(map) => {
                    if map.is_empty() {
                        out.push_str("{}");
                        continue;
                    }
                    out.push('{');
                    stack.push(Task::Literal("}"));
                    let entries: Vec<(&String, &JsonValue)> = map.iter().collect();
                    for index in (0..entries.len()).rev() {
                        stack.push(Task::Value(entries[index].1));
                        stack.push(Task::Key(entries[index].0.as_str()));
                        if index > 0 {
                            stack.push(Task::Literal(","));
                        }
                    }
                }
            },
        }
    }
    out
}

/// Dismantle a JSON value iteratively.
///
/// `serde_json::Value`'s drop glue recurses one frame per nesting level, so
/// dropping a payload that carries a session tree can overrun the stack of
/// whichever thread owns it (a long session is a chain as deep as it is long).
/// Moving each container's children onto an explicit worklist keeps the
/// teardown at constant stack depth.
pub fn drop_value_iteratively(value: JsonValue) {
    let mut pending: Vec<JsonValue> = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            JsonValue::Array(items) => pending.extend(items),
            JsonValue::Object(map) => {
                pending.extend(map.into_iter().map(|(_key, value)| value));
            }
            _ => {}
        }
    }
}

/// Append one `"key":value` pair to a hand-assembled JSON object, writing the
/// separating comma whenever this is not the first field.
///
/// `serde_json` encodes both the key and the value, so the text matches what
/// serializing the equivalent `Value` would produce; an unencodable value
/// degrades to `null`.
pub fn push_field<T: Serialize + ?Sized>(out: &mut String, first: &mut bool, key: &str, value: &T) {
    if *first {
        *first = false;
    } else {
        out.push(',');
    }
    out.push_str(&leaf_text(key));
    out.push(':');
    match serde_json::to_string(value) {
        Ok(text) => out.push_str(&text),
        Err(_) => out.push_str("null"),
    }
}

/// `serde_json` text of one leaf value (`null` when it cannot be encoded).
fn leaf_text<T: Serialize + ?Sized>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        Err(_) => String::from("null"),
    }
}
