//! `durable::harness::usage` — the `pi.usage` ledger's pure helpers.
//!
//! Port of upstream `packages/durable/src/harness/usage.ts`: inserting and
//! accumulating one bucket, saturating counter addition, merging whole states,
//! the serde shape (`cacheRead`/`cacheWrite1h`/`totalTokens`, camelCase cost
//! fields) and the fact that a tool named `__proto__` or `toString` is an
//! ordinary [`BTreeMap`] key in Rust.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use pir::ai::types::{Cost, Usage};
use pir::durable::harness::usage::{
    add_usage, add_usage_state, usage_add, UsageScope, UsageState, USAGE_KIND, USAGE_VERSION,
};
use serde_json::json;

// ── helpers ────────────────────────────────────────────────────────────────

fn cost(input: f64, output: f64, cache_read: f64, cache_write: f64, total: f64) -> Cost {
    Cost {
        input,
        output,
        cache_read,
        cache_write,
        total,
        ..Cost::default()
    }
}

/// A usage with the five required counters and no optional ones.
fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, total: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: total,
        cost: Cost::default(),
    }
}

// ── insertion and accumulation ─────────────────────────────────────────────

#[test]
fn first_add_inserts_and_second_add_accumulates() {
    let mut state = UsageState::new();
    let first = usage(1, 2, 3, 4, 10);

    usage_add(&mut state, UsageScope::Models, "openai/gpt", &first);

    assert_eq!(state.models.get("openai/gpt"), Some(&first));
    assert!(state.tools.is_empty());
    assert!(state.bucket(UsageScope::Models).contains_key("openai/gpt"));

    usage_add(
        &mut state,
        UsageScope::Models,
        "openai/gpt",
        &usage(10, 20, 30, 40, 100),
    );

    let total = state.models.get("openai/gpt").expect("the bucket entry");
    assert_eq!(total.input, 11);
    assert_eq!(total.output, 22);
    assert_eq!(total.cache_read, 33);
    assert_eq!(total.cache_write, 44);
    assert_eq!(total.total_tokens, 110);

    // The same key in the other scope is a separate entry.
    usage_add(&mut state, UsageScope::Tools, "openai/gpt", &first);
    assert_eq!(state.tools.get("openai/gpt"), Some(&first));
    assert_eq!(state.models.len(), 1);
    assert_eq!(state.tools.len(), 1);
}

#[test]
fn a_zero_usage_insert_is_a_zero_entry_not_a_missing_one() {
    let mut state = UsageState::new();

    usage_add(&mut state, UsageScope::Tools, "noop", &Usage::default());

    let entry = state.tools.get("noop").expect("the entry exists");
    assert_eq!(*entry, Usage::default());
    assert!(state.bucket_mut(UsageScope::Tools).contains_key("noop"));
}

#[test]
fn add_usage_adds_optional_counters_once_either_side_reports_them() {
    let cases = [
        (None, None, None),
        (Some(5), None, Some(5)),
        (None, Some(7), Some(7)),
        (Some(5), Some(7), Some(12)),
    ];

    for (total_optional, value_optional, expected) in cases {
        let mut total = Usage {
            cache_write_1h: total_optional,
            reasoning: total_optional,
            ..Usage::default()
        };
        let addition = Usage {
            cache_write_1h: value_optional,
            reasoning: value_optional,
            ..Usage::default()
        };

        add_usage(&mut total, &addition);

        assert_eq!(total.cache_write_1h, expected);
        assert_eq!(total.reasoning, expected);
    }
}

#[test]
fn add_usage_adds_every_cost_field() {
    let mut total = Usage {
        cost: cost(0.25, 0.5, 0.0, 0.125, 0.875),
        ..Usage::default()
    };
    let addition = Usage {
        cost: cost(0.25, 0.25, 0.5, 0.125, 1.125),
        ..Usage::default()
    };

    add_usage(&mut total, &addition);

    // All values are binary-exact, so the sums are exact too.
    assert_eq!(total.cost.input, 0.5);
    assert_eq!(total.cost.output, 0.75);
    assert_eq!(total.cost.cache_read, 0.5);
    assert_eq!(total.cost.cache_write, 0.25);
    assert_eq!(total.cost.total, 2.0);
}

#[test]
fn add_usage_saturates_instead_of_overflowing() {
    let mut total = Usage {
        input: u64::MAX,
        output: 1,
        cache_read: u64::MAX,
        cache_write: u64::MAX,
        cache_write_1h: Some(u64::MAX),
        reasoning: Some(u64::MAX),
        total_tokens: u64::MAX,
        cost: Cost::default(),
    };
    let addition = Usage {
        input: 5,
        output: u64::MAX,
        cache_read: 5,
        cache_write: 5,
        cache_write_1h: Some(1),
        reasoning: Some(1),
        total_tokens: 5,
        cost: Cost::default(),
    };

    add_usage(&mut total, &addition);

    assert_eq!(total.input, u64::MAX);
    assert_eq!(total.output, u64::MAX);
    assert_eq!(total.cache_read, u64::MAX);
    assert_eq!(total.cache_write, u64::MAX);
    assert_eq!(total.cache_write_1h, Some(u64::MAX));
    assert_eq!(total.reasoning, Some(u64::MAX));
    assert_eq!(total.total_tokens, u64::MAX);
    assert_eq!(total.cost, Cost::default());
}

#[test]
fn add_usage_state_merges_both_buckets() {
    let mut sum = UsageState::new();
    usage_add(&mut sum, UsageScope::Models, "a", &usage(1, 1, 1, 1, 1));
    usage_add(&mut sum, UsageScope::Tools, "t", &usage(2, 2, 2, 2, 2));

    let mut state = UsageState::new();
    usage_add(
        &mut state,
        UsageScope::Models,
        "a",
        &usage(10, 10, 10, 10, 10),
    );
    usage_add(
        &mut state,
        UsageScope::Models,
        "b",
        &usage(20, 20, 20, 20, 20),
    );
    usage_add(
        &mut state,
        UsageScope::Tools,
        "t",
        &usage(30, 30, 30, 30, 30),
    );
    usage_add(
        &mut state,
        UsageScope::Tools,
        "u",
        &usage(40, 40, 40, 40, 40),
    );

    add_usage_state(&mut sum, &state);

    assert_eq!(sum.models["a"].input, 11);
    assert_eq!(sum.models["b"].input, 20);
    assert_eq!(sum.tools["t"].input, 32);
    assert_eq!(sum.tools["u"].input, 40);
    assert_eq!(sum.models.len(), 2);
    assert_eq!(sum.tools.len(), 2);
}

// ── serde ──────────────────────────────────────────────────────────────────

#[test]
fn usage_state_round_trips_with_camel_case_fields() {
    let mut state = UsageState::new();
    let entry = Usage {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        cache_write_1h: Some(5),
        reasoning: Some(6),
        total_tokens: 10,
        cost: cost(0.1, 0.2, 0.3, 0.4, 0.5),
    };
    usage_add(&mut state, UsageScope::Models, "provider/model", &entry);

    let value = serde_json::to_value(&state).expect("serialize");
    let bucket = &value["models"]["provider/model"];

    assert_eq!(bucket["input"], json!(1));
    assert_eq!(bucket["output"], json!(2));
    assert_eq!(bucket["cacheRead"], json!(3));
    assert_eq!(bucket["cacheWrite"], json!(4));
    assert_eq!(bucket["cacheWrite1h"], json!(5));
    assert_eq!(bucket["reasoning"], json!(6));
    assert_eq!(bucket["totalTokens"], json!(10));
    assert_eq!(bucket["cost"]["cacheRead"], json!(0.3));
    assert_eq!(bucket["cost"]["cacheWrite"], json!(0.4));

    // The Rust field names never leak onto the wire.
    assert!(bucket.get("cache_read").is_none());
    assert!(bucket.get("total_tokens").is_none());

    let back: UsageState = serde_json::from_value(value).expect("deserialize");
    assert_eq!(back, state);
}

#[test]
fn absent_optional_counters_are_omitted_from_the_wire() {
    let mut state = UsageState::new();
    usage_add(&mut state, UsageScope::Tools, "bash", &usage(1, 2, 3, 4, 5));

    let value = serde_json::to_value(&state).expect("serialize");
    let bucket = &value["tools"]["bash"];

    assert!(bucket.get("cacheWrite1h").is_none());
    assert!(bucket.get("reasoning").is_none());
}

#[test]
fn proto_and_to_string_are_ordinary_map_keys() {
    let mut state = UsageState::new();
    for key in ["__proto__", "toString", "constructor"] {
        usage_add(&mut state, UsageScope::Models, key, &usage(1, 1, 1, 1, 1));
        usage_add(&mut state, UsageScope::Tools, key, &usage(2, 2, 2, 2, 2));
    }

    assert_eq!(state.models.len(), 3);
    assert_eq!(state.tools.len(), 3);
    for key in ["__proto__", "toString", "constructor"] {
        assert_eq!(state.models[key].input, 1);
        assert_eq!(state.tools[key].input, 2);
    }

    let back: UsageState = serde_json::from_value(serde_json::to_value(&state).expect("serialize"))
        .expect("deserialize");
    assert_eq!(back, state);
}

#[test]
fn usage_scope_serializes_lowercase() {
    assert_eq!(
        serde_json::to_value(UsageScope::Models).expect("serialize"),
        json!("models")
    );
    assert_eq!(
        serde_json::to_value(UsageScope::Tools).expect("serialize"),
        json!("tools")
    );
    assert_eq!(
        serde_json::from_value::<UsageScope>(json!("models")).expect("deserialize"),
        UsageScope::Models
    );
    assert!(serde_json::from_value::<UsageScope>(json!("both")).is_err());
}

#[test]
fn kind_and_version_are_the_persisted_pi_usage_values() {
    assert_eq!(USAGE_KIND, "pi.usage");
    assert_eq!(USAGE_VERSION, 1);
}
