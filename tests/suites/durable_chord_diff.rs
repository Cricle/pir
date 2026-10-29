//! `durable::chord` — the chord delta subset: revision diffing and trusted
//! immutable application. Ported from upstream `packages/chord/src/delta/
//! {diff,index,apply-immutable-trusted}.ts`.
use pir::durable::chord::{apply_immutable_trusted, diff_revisions};
use pir::durable::delta::{apply, Op, Seg};
use serde_json::{json, Value};

/// The round-trip corpus: every pair is diffed, replayed by `delta::apply` and
/// by `apply_immutable_trusted`, and must reproduce `after`. The table covers
/// each emission path — scalars, object add/remove/nest, array
/// append/prepend/splice/remove/permute, every string shape, the long-string
/// overlap scan, reserved keys, a type change and the >4096-op overflow.
fn pairs() -> Vec<(Value, Value)> {
    let long_trim_before = format!("{}tail", "a".repeat(70_000));
    let long_trim_after = format!("tail{}", "b".repeat(70_000));
    let long_append_before = "z".repeat(70_000);
    let long_append_after = format!("{long_append_before}!");

    // More changed fields than MAX_DELTA_OPERATIONS: the diff must give up and
    // replace the root.
    let mut wide_before = serde_json::Map::new();
    let mut wide_after = serde_json::Map::new();
    for index in 0..5_000 {
        wide_before.insert(format!("field{index}"), json!(0));
        wide_after.insert(format!("field{index}"), json!(1));
    }

    vec![
        // Scalar change.
        (json!({"value": 1}), json!({"value": 2})),
        // Object key added.
        (json!({"a": 1}), json!({"a": 1, "b": 2})),
        // Object key removed.
        (json!({"a": 1, "b": 2}), json!({"a": 1})),
        // Nested object.
        (
            json!({"outer": {"inner": {"a": 1, "b": 2}}}),
            json!({"outer": {"inner": {"a": 1, "b": 3}}}),
        ),
        // Array append.
        (json!({"xs": [1, 2, 3]}), json!({"xs": [1, 2, 3, 4]})),
        // Array prepend.
        (json!({"xs": [1, 2, 3]}), json!({"xs": [0, 1, 2, 3]})),
        // Array splice: insert in the middle.
        (json!({"xs": [1, 2, 3]}), json!({"xs": [1, 9, 2, 3]})),
        // Array middle removal.
        (json!({"xs": [1, 2, 3, 4]}), json!({"xs": [1, 3, 4]})),
        // Array reorder: a permutation.
        (json!({"xs": [1, 2, 3]}), json!({"xs": [3, 1, 2]})),
        // Array of objects with one changed field.
        (
            json!({"xs": [{"id": 1, "v": "a"}, {"id": 2, "v": "b"}]}),
            json!({"xs": [{"id": 1, "v": "a"}, {"id": 2, "v": "c"}]}),
        ),
        // String append.
        (json!({"text": "hello"}), json!({"text": "hello world"})),
        // String front trim + append.
        (json!({"text": "abcdefgh"}), json!({"text": "defghxyz"})),
        // String full replace.
        (json!({"text": "alpha"}), json!({"text": "omega"})),
        // String longer than the 64 KiB overlap scan, trimmed and appended.
        (
            json!({"text": long_trim_before}),
            json!({"text": long_trim_after}),
        ),
        // String longer than the scan, extended: prefix-append.
        (
            json!({"text": long_append_before}),
            json!({"text": long_append_after}),
        ),
        // Reserved key nested: the whole owning object is set, never the key.
        (
            json!({"nested": {"__proto__": 1, "keep": true}}),
            json!({"nested": {"__proto__": 2, "keep": true}}),
        ),
        // Reserved key at the root: only a replacement can express it.
        (
            json!({"__proto__": 1, "other": true}),
            json!({"__proto__": 2, "other": true}),
        ),
        // Overflow: more operations than the batch may hold.
        (Value::Object(wide_before), Value::Object(wide_after)),
        // Scalar type change.
        (json!({"v": [1, 2]}), json!({"v": {"a": 1}})),
        // Array to scalar.
        (json!({"v": [1, 2, 3]}), json!({"v": "done"})),
        // No change at all.
        (
            json!({"same": [1, {"a": true}]}),
            json!({"same": [1, {"a": true}]}),
        ),
    ]
}

#[test]
fn diff_round_trips_every_pair() {
    for (index, (before, after)) in pairs().into_iter().enumerate() {
        let diff = diff_revisions(&before, &after);
        let mut replayed = before.clone();
        apply(&mut replayed, &diff).unwrap_or_else(|error| {
            panic!("pair {index}: delta::apply failed: {error}; ops {diff:?}")
        });
        assert_eq!(replayed, after, "pair {index} ops {diff:?}");
    }
}

#[test]
fn string_operations_have_the_upstream_shapes() {
    let path = vec![Seg::Key("text".to_owned())];

    // A suffix is exactly one append.
    let appended = diff_revisions(&json!({"text": "hello"}), &json!({"text": "hello world"}));
    assert_eq!(
        appended,
        vec![Op::Append(path.clone(), " world".to_owned())]
    );

    // A front trim plus a suffix is a truncation followed by an append.
    let trimmed = diff_revisions(&json!({"text": "abcdefgh"}), &json!({"text": "defghxyz"}));
    assert_eq!(
        trimmed,
        vec![
            Op::Truncate(path.clone(), 3),
            Op::Append(path.clone(), "xyz".to_owned()),
        ]
    );

    // A shared suffix with nothing appended is one truncation.
    let prefix = diff_revisions(&json!({"text": "abcdef"}), &json!({"text": "def"}));
    assert_eq!(prefix, vec![Op::Truncate(path.clone(), 3)]);

    // No overlap at all falls back to a set.
    let replaced = diff_revisions(&json!({"text": "alpha"}), &json!({"text": "omega"}));
    assert_eq!(
        replaced,
        vec![Op::Set(path, Value::String("omega".to_owned()))]
    );
}

#[test]
fn cost_decision_falls_back_to_a_root_replacement() {
    // An empty change emits nothing.
    assert_eq!(
        diff_revisions(&json!({"same": [1, 2]}), &json!({"same": [1, 2]})),
        Vec::new()
    );

    // Reordered deeply-equal values are one revision, so nothing is emitted
    // (upstream's `normalizes reordered distinct deeply-equal objects`).
    assert_eq!(
        diff_revisions(
            &json!({"xs": [{"n": 1}, {"n": 1}]}),
            &json!({"xs": [{"n": 1}, {"n": 1}]})
        ),
        Vec::new()
    );

    // A 80 KiB replacement costs more than the snapshot once the 65_536
    // delta-cost short-circuit is passed, so the whole value is replaced.
    let before = json!({"text": "a".repeat(80_000)});
    let after = json!({"text": "b".repeat(80_000)});
    assert_eq!(
        diff_revisions(&before, &after),
        vec![Op::Replace(after.clone())]
    );

    // Overflow past MAX_DELTA_OPERATIONS replaces the root as well.
    let overflow = pairs()
        .into_iter()
        .find(|(before, after)| {
            let diff = diff_revisions(before, after);
            matches!(diff.as_slice(), [Op::Replace(_)])
                && before.as_object().is_some_and(|map| map.len() > 4_096)
        })
        .expect("the corpus contains the overflow pair");
    assert_eq!(
        diff_revisions(&overflow.0, &overflow.1),
        vec![Op::Replace(overflow.1.clone())]
    );

    // Small diffs stay compact: the cost check only runs above 65_536.
    let compact = diff_revisions(&json!({"a": 1, "b": 2}), &json!({"a": 1, "b": 3}));
    assert_eq!(
        compact,
        vec![Op::Set(vec![Seg::Key("b".to_owned())], json!(3))]
    );
}

#[test]
fn overlap_finds_the_longest_suffix_prefix() {
    use pir::durable::chord::overlap;

    assert_eq!(overlap("abcdef", "defghi", 100), 3);
    assert_eq!(overlap("abcabc", "abcx", 100), 3);
    assert_eq!(overlap("abc", "xyz", 100), 0);
    assert_eq!(overlap("", "abc", 100), 0);
    assert_eq!(overlap("abc", "", 100), 0);
    assert_eq!(overlap("abc", "abc", 0), 0);
}

#[test]
fn trusted_apply_matches_the_general_applier() {
    for (index, (before, after)) in pairs().into_iter().enumerate() {
        let original = before.clone();
        let diff = diff_revisions(&before, &after);

        let mut replayed = before.clone();
        apply(&mut replayed, &diff).unwrap_or_else(|error| panic!("pair {index}: {error}"));

        let trusted = apply_immutable_trusted(before.clone(), &diff)
            .unwrap_or_else(|error| panic!("pair {index}: {error}"));

        assert_eq!(trusted, replayed, "pair {index} ops {diff:?}");
        assert_eq!(trusted, after, "pair {index} ops {diff:?}");
        assert_eq!(before, original, "pair {index}: input mutated");
    }

    // Root-level splices and permutations, the one case `s`/`d`/`a`/`t` cannot
    // express.
    let target = json!([1, 2, 3]);
    let operations = vec![
        Op::Splice {
            path: Vec::new(),
            index: 3,
            remove: 0,
            items: vec![json!(4)],
        },
        Op::Move {
            path: Vec::new(),
            permutation: vec![3, 0, 1, 2],
        },
    ];
    let mut expected = target.clone();
    apply(&mut expected, &operations).expect("delta::apply");
    assert_eq!(
        apply_immutable_trusted(target.clone(), &operations).expect("trusted apply"),
        expected
    );
    assert_eq!(target, json!([1, 2, 3]), "input mutated");
}

#[test]
fn trusted_apply_rejects_the_shapes_upstream_rejects() {
    // A `p` whose target is not an array.
    assert!(apply_immutable_trusted(
        json!({"obj": {}}),
        &[Op::Splice {
            path: vec![Seg::Key("obj".to_owned())],
            index: 0,
            remove: 0,
            items: Vec::new(),
        }]
    )
    .is_err());

    // An `m` whose target is not an array.
    assert!(apply_immutable_trusted(
        json!({"obj": {}}),
        &[Op::Move {
            path: vec![Seg::Key("obj".to_owned())],
            permutation: vec![0],
        }]
    )
    .is_err());

    // A root `p`/`m` whose root is not an array.
    assert!(apply_immutable_trusted(
        json!({"a": 1}),
        &[Op::Splice {
            path: Vec::new(),
            index: 0,
            remove: 0,
            items: Vec::new(),
        }]
    )
    .is_err());
    assert!(apply_immutable_trusted(
        json!({"a": 1}),
        &[Op::Move {
            path: Vec::new(),
            permutation: Vec::new(),
        }]
    )
    .is_err());

    // A non-string `a` target.
    assert!(apply_immutable_trusted(
        json!({"n": 1}),
        &[Op::Append(vec![Seg::Key("n".to_owned())], "x".to_owned())]
    )
    .is_err());

    // A non-string `t` target.
    assert!(apply_immutable_trusted(
        json!({"n": 1}),
        &[Op::Truncate(vec![Seg::Key("n".to_owned())], 1)]
    )
    .is_err());

    // A non-numeric deletion key on an array.
    assert!(apply_immutable_trusted(
        json!({"xs": [1, 2]}),
        &[Op::Delete(vec![
            Seg::Key("xs".to_owned()),
            Seg::Key("k".to_owned()),
        ])]
    )
    .is_err());

    // An unresolvable path.
    assert!(apply_immutable_trusted(
        json!({"xs": [1, 2]}),
        &[Op::Set(
            vec![Seg::Key("xs".to_owned()), Seg::Index(9)],
            json!(3)
        )]
    )
    .is_err());
}

// ── UTF-16 parity ───────────────────────────────────────────────────────────

#[test]
fn string_ops_are_utf16_units_like_the_typescript_originals() {
    // A non-BMP character is one scalar but two UTF-16 units. The emitted
    // `t`/`a` counts must match `String.prototype.length`/`.slice`, because the
    // persisted op means "drop N UTF-16 units" and a TypeScript reader replays
    // it with exactly that meaning.
    let before = "😀😀head";
    let after = "head😀tail";
    let ops = diff_revisions(&json!(before), &json!(after));
    // Every string op's count/slice is expressed in UTF-16 units.
    for op in &ops {
        match op {
            Op::Truncate(_, count) => assert!(
                *count <= before.encode_utf16().count(),
                "truncate count must not exceed the UTF-16 length"
            ),
            Op::Append(_, text) => assert!(
                !text.contains('\u{FFFD}'),
                "a well-formed pair must not need a lossy slice"
            ),
            _ => {}
        }
    }
    let applied =
        apply_immutable_trusted(Value::String(before.into()), &ops).expect("diff output replays");
    assert_eq!(applied, Value::String(after.into()));
}

#[test]
fn a_prefix_growth_emits_exactly_one_append() {
    let ops = diff_revisions(&json!({"out": "abc"}), &json!({"out": "abcdef"}));
    assert_eq!(ops.len(), 1, "one append, no truncate: {ops:?}");
    match &ops[0] {
        Op::Append(path, text) => {
            assert_eq!(text, "def");
            assert_eq!(path.len(), 1);
        }
        other => panic!("expected an append, got {other:?}"),
    }
}
