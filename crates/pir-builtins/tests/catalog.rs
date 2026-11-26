//! The catalog's order, default set and feature mapping are the product
//! contract; they must match the historical in-core builtin table exactly.

use pir_builtins::*;

const EXPECTED_ORDER: [&str; 11] = [
    "subagent",
    "mode",
    "mcp",
    "codemode",
    "python",
    "context-mode",
    "tool-search",
    "tool-repair",
    "todo",
    "memory",
    "phone",
];

#[test]
fn known_table_is_the_historical_order() {
    let names: Vec<&str> = KNOWN_BUILTINS.iter().map(|row| row.name).collect();
    assert_eq!(names, EXPECTED_ORDER.to_vec());
}

#[test]
fn catalog_order_is_stable() {
    // `python` is a known row without a factory here (its implementation needs
    // the engine's agent traits and lives in `pir-core`), so the enabled
    // catalog is the table minus that one row, in the same order.
    let expected: Vec<&str> = EXPECTED_ORDER
        .iter()
        .copied()
        .filter(|name| *name != "python")
        .collect();
    assert_eq!(names(), expected);
}

#[test]
fn descriptors_are_a_projection_of_the_known_table() {
    for entry in descriptors() {
        let row = known(entry.name).expect("every descriptor has a known row");
        assert_eq!(entry.feature, row.host_feature, "{}", entry.name);
        assert_eq!(entry.default_enabled, row.default_enabled, "{}", entry.name);
        assert!(entry.replaceable, "{} must stay replaceable", entry.name);
    }
    // ... and nothing in the table is silently missing from a full build.
    // `python` is the documented exception: `pir-core` owns its factory.
    let missing: Vec<&str> = KNOWN_BUILTINS
        .iter()
        .map(|row| row.name)
        .filter(|name| !names().contains(name))
        .collect();
    assert_eq!(missing, vec!["python"], "full build must project every row");
}

#[test]
fn phone_is_the_only_default_off_builtin() {
    let defaults = default_names();
    assert!(!defaults.contains(&"phone"));
    // Every default-on row except `phone` is instantiable here, and `python`
    // is default-on but owned by `pir-core`, so it is absent from `defaults`.
    assert_eq!(defaults.len(), EXPECTED_ORDER.len() - 2);
    let off: Vec<&str> = KNOWN_BUILTINS
        .iter()
        .filter(|row| !row.default_enabled)
        .map(|row| row.name)
        .collect();
    assert_eq!(off, vec!["phone"]);
}

#[test]
fn every_descriptor_instantiates_with_its_builtin_identity() {
    for entry in descriptors() {
        let plugin = (entry.factory)();
        assert_eq!(plugin.info().path, format!("builtin:{}", entry.name));
    }
}

#[test]
fn lookup_and_instantiate_agree() {
    for name in names() {
        assert!(descriptor(name).is_some(), "{name}");
        assert!(instantiate(name).is_some(), "{name}");
    }
    assert!(descriptor("nope").is_none());
    assert!(instantiate("nope").is_none());
    assert!(known("nope").is_none());
}
