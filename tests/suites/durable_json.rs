//! `durable::harness::json` — leaf-by-leaf assignment and its append guarantee.
//!
//! Port of upstream `packages/durable/src/harness/json.ts`. Chord records a
//! container assignment as one full `Set`, so a partial that is written whole
//! would store and publish the complete message on every flush. `assign_json`
//! walks the stored value and the new value together, leaf by leaf, so the
//! differ sees the individual leaves; a growing string leaf then becomes one
//! [`Op::Append`] instead of a container `Set`/`Replace`.
//!
//! The rules, in upstream order:
//!
//! * object × object — delete keys absent from `value`, then recurse in
//!   `value`'s key order (retained keys keep their positions, new keys append);
//! * array × array when `current.len() <= value.len()` — recurse by index,
//!   push the tail;
//! * otherwise — replace the slot unless the values are deeply equal.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use pir::durable::delta::{apply, Op, Seg};
use pir::durable::harness::json::{assign_json, assign_json_at, assign_then_diff};
use serde_json::{json, Value};

// ── helpers ────────────────────────────────────────────────────────────────

/// `assign_json` on a document known to be an object.
fn assign(target: &mut Value, key: &str, value: Value) {
    assign_json(
        target.as_object_mut().expect("target is an object"),
        key,
        value,
    );
}

/// The keys of an object value, in serialization order.
fn keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("value is an object")
        .keys()
        .cloned()
        .collect()
}

// ── object × object ────────────────────────────────────────────────────────

#[test]
fn object_object_deletes_missing_keys_and_recurses() {
    let mut target = json!({
        "message": {
            "role": "assistant",
            "content": "hi",
            "extra": { "old": 1, "keep": 2 }
        },
        "other": true
    });

    assign(
        &mut target,
        "message",
        json!({
            "role": "assistant",
            "content": "hello",
            "new": true,
            "extra": { "keep": 3, "added": 4 }
        }),
    );

    // `content` was replaced, `extra.old` was deleted, `extra.added` was
    // inserted, and the sibling `other` was untouched.
    assert_eq!(
        target,
        json!({
            "message": {
                "role": "assistant",
                "content": "hello",
                "new": true,
                "extra": { "keep": 3, "added": 4 }
            },
            "other": true
        })
    );
}

#[test]
fn object_object_retains_positions_and_appends_new_keys() {
    let mut target = json!({ "message": { "first": 1, "second": 2 } });

    // `second` and `first` already exist and keep their positions; `third` is
    // new and appends, even though `value` lists it before `first`.
    assign(
        &mut target,
        "message",
        json!({ "second": 20, "third": 3, "first": 10 }),
    );

    assert_eq!(
        keys(&target["message"]),
        vec!["first".to_owned(), "second".to_owned(), "third".to_owned()]
    );
    assert_eq!(
        target["message"],
        json!({ "first": 10, "second": 20, "third": 3 })
    );
}

#[test]
fn empty_object_value_deletes_every_key() {
    let mut target = json!({ "message": { "a": 1, "b": { "c": 2 } } });

    assign(&mut target, "message", json!({}));

    assert_eq!(target["message"], json!({}));
    assert!(keys(&target["message"]).is_empty());
}

#[test]
fn absent_key_is_inserted_whole() {
    let mut target = json!({ "seq": 1 });

    assign(&mut target, "message", json!({ "role": "assistant" }));

    assert_eq!(
        target,
        json!({ "seq": 1, "message": { "role": "assistant" } })
    );
}

// ── array × array ──────────────────────────────────────────────────────────

#[test]
fn array_array_recurses_by_index_and_appends_the_tail() {
    let mut target = json!({
        "message": {
            "content": [1, { "a": 1, "keep": true }, "x"]
        }
    });

    assign(
        &mut target,
        "message",
        json!({
            "content": [1, { "a": 2, "b": 3 }, "x", 4, 5]
        }),
    );

    assert_eq!(
        target["message"]["content"],
        json!([1, { "a": 2, "b": 3 }, "x", 4, 5])
    );
}

#[test]
fn longer_current_array_is_replaced_whole() {
    let mut target = json!({ "message": { "content": [1, 2, 3] } });

    assign(&mut target, "message", json!({ "content": [9] }));

    // `current.len() > value.len()` takes the replacement path rather than
    // recursing into the shared prefix and truncating.
    assert_eq!(target["message"]["content"], json!([9]));
}

#[test]
fn equal_length_arrays_recurse() {
    let mut target = json!({ "message": { "content": [{ "text": "a" }] } });

    assign(
        &mut target,
        "message",
        json!({ "content": [{ "text": "b" }] }),
    );

    assert_eq!(target["message"]["content"], json!([{ "text": "b" }]));
}

#[test]
fn nested_arrays_merge_leaf_by_leaf() {
    let mut target = json!({ "message": { "items": [[1, 2], [3]] } });

    assign(
        &mut target,
        "message",
        json!({ "items": [[1, 9], [3, 4, 5]] }),
    );

    assert_eq!(target["message"]["items"], json!([[1, 9], [3, 4, 5]]));
}

#[test]
fn assign_json_at_fills_gaps_with_null() {
    let mut items = vec![json!("a")];

    assign_json_at(&mut items, 3, json!({ "b": 1 }));

    assert_eq!(
        items,
        vec![json!("a"), Value::Null, Value::Null, json!({ "b": 1 })]
    );
}

#[test]
fn assign_json_at_recurses_into_existing_slot() {
    let mut items = vec![json!({ "a": 1, "old": true })];

    assign_json_at(&mut items, 0, json!({ "a": 2 }));

    assert_eq!(items, vec![json!({ "a": 2 })]);
}

// ── other values ───────────────────────────────────────────────────────────

#[test]
fn equal_values_are_left_alone() {
    let before = json!({ "message": { "role": "assistant", "content": [1, 2] } });

    // Assigning an equal (but separate) value produces no diff at all.
    assert!(assign_then_diff(&before, &before["message"], "message").is_empty());

    let mut target = json!({ "message": { "role": "assistant" } });
    assign(&mut target, "message", json!({ "role": "assistant" }));
    assert_eq!(target, json!({ "message": { "role": "assistant" } }));
}

#[test]
fn scalar_slot_is_replaced_when_different() {
    let mut target = json!({ "message": { "text": "short", "count": 1 } });

    assign(
        &mut target,
        "message",
        json!({ "text": "a different string", "count": 2 }),
    );

    assert_eq!(
        target["message"],
        json!({ "text": "a different string", "count": 2 })
    );
}

#[test]
fn non_object_current_is_replaced_by_an_object() {
    let mut target = json!({ "message": null });

    assign(&mut target, "message", json!({ "role": "assistant" }));

    assert_eq!(target["message"], json!({ "role": "assistant" }));
}

// ── the append guarantee ───────────────────────────────────────────────────

#[test]
fn growing_message_string_is_an_append_not_a_container_set() {
    let before = json!({
        "message": {
            "role": "assistant",
            "content": [{ "type": "text", "text": "Hello" }]
        },
        "seq": 7
    });
    let after = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "Hello, world" }]
    });

    let ops = assign_then_diff(&before, &after, "message");

    // One append at the string leaf; never a `Set`/`Replace` of `message` or
    // of its content array.
    assert_eq!(ops.len(), 1, "expected one append, got {ops:?}");
    match &ops[0] {
        Op::Append(path, text) => {
            assert_eq!(
                path,
                &vec![
                    Seg::Key("message".to_owned()),
                    Seg::Key("content".to_owned()),
                    Seg::Index(0),
                    Seg::Key("text".to_owned()),
                ]
            );
            assert_eq!(text, ", world");
        }
        other => panic!("growing a string leaf must be an append, got {other:?}"),
    }

    // The batch replays into the leaf-assigned document.
    let mut replay = before.clone();
    apply(&mut replay, &ops).expect("the append batch applies");
    assert_eq!(
        replay["message"],
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "Hello, world" }]
        })
    );
    assert_eq!(replay["seq"], json!(7));
}

#[test]
fn non_object_before_has_no_diff() {
    // There is no slot to assign into, so `assign_then_diff` returns nothing.
    for before in [json!([1, 2, 3]), json!("text"), json!(7), Value::Null] {
        let ops = assign_then_diff(&before, &json!({ "role": "assistant" }), "message");
        assert!(ops.is_empty(), "before={before} produced {ops:?}");
    }
}

#[test]
fn missing_slot_diffs_as_one_set() {
    let before = json!({ "seq": 1 });
    let after = json!({ "role": "assistant" });

    let ops = assign_then_diff(&before, &after, "message");

    // A slot that did not exist has no current value to merge with; it is
    // inserted whole, which the differ sees as one `Set`.
    assert_eq!(ops.len(), 1, "got {ops:?}");
    match &ops[0] {
        Op::Set(path, value) => {
            assert_eq!(path, &vec![Seg::Key("message".to_owned())]);
            assert_eq!(value, &after);
        }
        other => panic!("an absent slot must be set whole, got {other:?}"),
    }
}
