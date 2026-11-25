//! The catalog's order, default set and feature mapping are the product
//! contract; they must match the historical in-core builtin table exactly.

use pir_builtins::*;

const EXPECTED_ORDER: [&str; 10] = [
    "subagent",
    "mode",
    "mcp",
    "codemode",
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
    assert_eq!(names(), EXPECTED_ORDER.to_vec());
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
    assert_eq!(descriptors().len(), KNOWN_BUILTINS.len());
}

#[test]
fn phone_is_the_only_default_off_builtin() {
    let defaults = default_names();
    assert!(!defaults.contains(&"phone"));
    assert_eq!(defaults.len(), EXPECTED_ORDER.len() - 1);
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
