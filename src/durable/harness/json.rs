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

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::durable::chord::diff_revisions;
use crate::durable::delta::Op;

/// Key of one planned container member or item.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanKey {
    /// An object member.
    Key(String),
    /// An array item.
    Index(usize),
}

/// How [`assign_json_with_plan`] writes one value instead of assigning it leaf
/// by leaf (upstream `AssignPlan`).
#[derive(Clone, Debug, PartialEq)]
pub enum AssignPlan {
    /// The string grew by `text` since it was last assigned: append it, when
    /// the lengths agree.
    Append {
        /// The text appended since the last assignment.
        text: String,
    },
    /// Leave the value as it is; the caller writes it.
    Keep,
    /// Plans for some members or items of a container.
    Fields(BTreeMap<PlanKey, AssignPlan>),
}

impl AssignPlan {
    /// The plan of one member or item, when this plan names it.
    fn field(&self, key: &PlanKey) -> Option<&AssignPlan> {
        match self {
            Self::Fields(fields) => fields.get(key),
            Self::Append { .. } | Self::Keep => None,
        }
    }
}

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
    assign_json_with_plan(target, key, value, None);
}

/// [`assign_json`] with an append plan (upstream `assignJson(..., plan)`).
///
/// Where `plan` names a string member that grew by some text and the committed
/// string plus that text has the new string's length, the text is appended
/// instead of assigning the whole string. A [`AssignPlan::Keep`] leaves the
/// slot untouched; the caller writes it.
pub fn assign_json_with_plan(
    target: &mut Map<String, Value>,
    key: &str,
    value: Value,
    plan: Option<&AssignPlan>,
) {
    if matches!(plan, Some(AssignPlan::Keep)) {
        return;
    }
    match target.get_mut(key) {
        // Upstream reads `undefined` for an absent key: neither a record nor an
        // array, and unequal to every JSON value, so the slot is set whole.
        None => {
            target.insert(key.to_owned(), value);
        }
        Some(current) => assign_value(current, value, plan),
    }
}

/// Array form for a value already inside an array slot.
///
/// Mirrors [`assign_json`] for an index instead of a key. An index at or past
/// the end extends the array; the gap up to it is filled with `Value::Null`,
/// which is what JSON serialization produces for upstream's sparse array holes.
pub fn assign_json_at(target: &mut Vec<Value>, index: usize, value: Value) {
    assign_json_at_with_plan(target, index, value, None);
}

/// [`assign_json_at`] with an append plan.
pub fn assign_json_at_with_plan(
    target: &mut Vec<Value>,
    index: usize,
    value: Value,
    plan: Option<&AssignPlan>,
) {
    if matches!(plan, Some(AssignPlan::Keep)) {
        return;
    }
    if index >= target.len() {
        target.resize(index + 1, Value::Null);
    }
    assign_value(&mut target[index], value, plan);
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
fn assign_value(current: &mut Value, value: Value, plan: Option<&AssignPlan>) {
    match (current, value) {
        (Value::Object(current), Value::Object(value)) => {
            assign_object(current, value, plan);
        }
        (Value::Array(current), Value::Array(value)) if current.len() <= value.len() => {
            assign_array(current, value, plan);
        }
        (current, value) => assign_leaf(current, value, plan),
    }
}

/// Leaf branch: append a string that grew by the plan's text when the lengths
/// agree, otherwise replace unless the values are equal.
fn assign_leaf(current: &mut Value, value: Value, plan: Option<&AssignPlan>) {
    if let Some(AssignPlan::Append { text }) = plan {
        if let (Value::String(current), Value::String(value)) = (&mut *current, &value) {
            if current.len() + text.len() == value.len() {
                current.push_str(text);
                return;
            }
        }
    }
    if *current != value {
        *current = value;
    }
}

/// Object branch: delete keys the right side dropped, then recurse in the
/// right side's key order. Retained keys keep their positions, exactly as
/// `Object.keys`/`delete` and property assignment behave upstream; new keys are
/// appended in `value`'s order.
fn assign_object(
    current: &mut Map<String, Value>,
    value: Map<String, Value>,
    plan: Option<&AssignPlan>,
) {
    current.retain(|key, _| value.contains_key(key.as_str()));
    for (key, child) in value {
        let child_plan = plan.and_then(|plan| plan.field(&PlanKey::Key(key.clone())));
        assign_json_with_plan(current, &key, child, child_plan);
    }
}

/// Array branch: recurse over the shared prefix and push the tail. The caller
/// guarantees `current.len() <= value.len()`; a longer current array takes the
/// whole-slot replacement path instead.
fn assign_array(current: &mut Vec<Value>, value: Vec<Value>, plan: Option<&AssignPlan>) {
    for (index, child) in value.into_iter().enumerate() {
        if index < current.len() {
            let child_plan = plan.and_then(|plan| plan.field(&PlanKey::Index(index)));
            assign_json_at_with_plan(current, index, child, child_plan);
        } else {
            current.push(child);
        }
    }
}
