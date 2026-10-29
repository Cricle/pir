//! Leaf-by-leaf JSON assignment: the port of upstream
//! `packages/durable/src/harness/json.ts`.
//!
//! Chord records a **container assignment as one full set** and only emits an
//! append when a string leaf is reassigned with a longer string. The live
//! documents therefore cannot be written by replacing a partial container
//! whole: every flush would store and publish the complete message. Assigning
//! leaf by leaf instead lets the differ see the individual leaves, so a growing
//! message becomes one [`Op::Append`](crate::durable::delta::Op::Append).
//!
//! The rules are upstream's, in its order:
//!
//! * both sides are objects: delete keys absent from `value`, then recurse over
//!   `value`'s entries in `value`'s key order;
//! * both sides are arrays and the current one is not longer: recurse by index
//!   and push the remainder;
//! * otherwise replace the slot, unless the values are equal.
//!
//! Keys are strings here (JSON objects); array indices are `usize`. Equality is
//! deep equality, the same stand-in for upstream's reference identity used by
//! [`crate::durable::chord::diff_revisions`].

use serde_json::{Map, Value};

use crate::durable::chord::diff_revisions;
use crate::durable::delta::Op;

/// Assign `value` into `target[key]` leaf by leaf.
///
/// Port of upstream `assignJson` (`harness/json.ts:9-27`). Chord records a
/// container assignment as one full set and only emits an append when a string
/// leaf is reassigned with a longer string, so writing the partial whole would
/// store and publish the complete message on every flush.
///
/// - both sides are objects: delete keys absent from `value`, then recurse in
///   `value`'s key order;
/// - both sides are arrays and the current one is not longer: recurse by index,
///   push the remainder;
/// - otherwise replace unless the values are equal.
///
/// Keys are strings here (JSON objects); indices are `usize`.
pub fn assign_json(target: &mut Map<String, Value>, key: &str, value: Value) {
    match target.get_mut(key) {
        // Upstream reads `undefined` for an absent key: neither a record nor an
        // array, and unequal to every JSON value, so the slot is set whole.
        None => {
            target.insert(key.to_owned(), value);
        }
        Some(current) => assign_value(current, value),
    }
}

/// Array form for a value already inside an array slot.
///
/// Mirrors [`assign_json`] for an index instead of a key. An index at or past
/// the end extends the array; the gap up to it is filled with `Value::Null`,
/// which is what JSON serialization produces for upstream's sparse array holes.
pub fn assign_json_at(target: &mut Vec<Value>, index: usize, value: Value) {
    if index >= target.len() {
        target.resize(index + 1, Value::Null);
    }
    assign_value(&mut target[index], value);
}

/// Diff two values after `assign_json` to show the resulting ops are appends.
///
/// `before` is the stored document and `after` the new value of its
/// `path_key` slot (usually a partial container such as the message being
/// generated). The document is cloned, `after` is assigned into the clone leaf
/// by leaf, and the clone is diffed against `before`: a growing string leaf
/// therefore comes out as one [`Op::Append`] rather than a full
/// [`Op::Set`] of the container.
///
/// A non-object `before` has no slot to assign into, so the diff is empty.
pub fn assign_then_diff(before: &Value, after: &Value, path_key: &str) -> Vec<Op> {
    let mut assigned = before.clone();
    if let Value::Object(target) = &mut assigned {
        assign_json(target, path_key, after.clone());
    }
    diff_revisions(before, &assigned)
}

/// `assignJson` on one already-resolved slot.
fn assign_value(current: &mut Value, value: Value) {
    match (current, value) {
        (Value::Object(current), Value::Object(value)) => {
            assign_object(current, value);
        }
        (Value::Array(current), Value::Array(value)) if current.len() <= value.len() => {
            assign_array(current, value);
        }
        (current, value) => {
            if *current != value {
                *current = value;
            }
        }
    }
}

/// Object branch: delete keys the right side dropped, then recurse in the
/// right side's key order. Retained keys keep their positions, exactly as
/// `Object.keys`/`delete` and property assignment behave upstream; new keys are
/// appended in `value`'s order.
fn assign_object(current: &mut Map<String, Value>, value: Map<String, Value>) {
    current.retain(|key, _| value.contains_key(key.as_str()));
    for (key, child) in value {
        assign_json(current, &key, child);
    }
}

/// Array branch: recurse over the shared prefix and push the tail. The caller
/// guarantees `current.len() <= value.len()`; a longer current array takes the
/// whole-slot replacement path instead.
fn assign_array(current: &mut Vec<Value>, value: Vec<Value>) {
    for (index, child) in value.into_iter().enumerate() {
        if index < current.len() {
            assign_json_at(current, index, child);
        } else {
            current.push(child);
        }
    }
}
