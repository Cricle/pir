//! The engine's builtin factory table and the `pir-builtins` catalog must
//! describe the same builtins, in the same order, with the same flags.
//!
//! Both tables exist on purpose during the extraction: `pir-builtins` owns the
//! metadata (order, default-enable, host feature) for the independent crates,
//! while `pir-core` still owns the concrete factories and their per-session
//! `rewire` hooks. Nothing but this test ties them together, so a row added to
//! one and not the other is a silent product change (a builtin that is listed
//! but never instantiated, or instantiated but never on the default plan).

use std::collections::BTreeSet;

use pir::plugins::builtin::{
    builtin_extension_def, builtin_names, BUILTIN_EXTENSION_FACTORIES, KNOWN_BUILTINS,
};

#[test]
fn factory_table_and_known_table_agree_on_order() {
    let factories: Vec<&str> = BUILTIN_EXTENSION_FACTORIES
        .iter()
        .map(|def| def.name)
        .collect();
    let known: Vec<&str> = KNOWN_BUILTINS.iter().map(|row| row.name).collect();
    assert_eq!(
        factories, known,
        "pir-core's factory table and pir-builtins::KNOWN_BUILTINS must list the same builtins in \
         the same order"
    );
}

#[test]
fn factory_table_and_known_table_agree_on_flags() {
    for def in BUILTIN_EXTENSION_FACTORIES {
        let row = KNOWN_BUILTINS
            .iter()
            .find(|row| row.name == def.name)
            .unwrap_or_else(|| panic!("{} has no known row", def.name));
        assert_eq!(
            def.default_enabled, row.default_enabled,
            "{} default_enabled disagrees between the factory table and the catalog",
            def.name
        );
        assert!(
            def.replaceable,
            "{} must stay replaceable (a user extension registering the same \
             tool/command omits the builtin)",
            def.name
        );
    }
}

#[test]
fn default_plan_flags_match_the_catalog() {
    // `default_builtin_paths_for` reads the factory table, so this is the
    // engine-side half of the same contract the crate-level catalog test pins.
    let default_on: BTreeSet<&str> = BUILTIN_EXTENSION_FACTORIES
        .iter()
        .filter(|def| def.default_enabled)
        .map(|def| def.name)
        .collect();
    assert_eq!(
        default_on,
        BTreeSet::from(["subagent", "mode", "todo"]),
        "the default plan is todo, subagent and mode only: every other builtin \
         must be named by a flag, a setting or a tool selection, because its \
         schema is paid for on every request"
    );
}

#[test]
fn every_factory_entry_resolves_by_name() {
    for def in BUILTIN_EXTENSION_FACTORIES {
        let resolved = builtin_extension_def(def.name)
            .unwrap_or_else(|| panic!("{} does not resolve from the active catalog", def.name));
        assert_eq!(resolved.name, def.name);
    }
    assert!(builtin_names().contains(&"mcp"));
}

/// The enabled name set is identical on both sides (order is pinned by the
/// first test; this one catches a name that is listed in only one table).
///
/// The catalog side is filtered by *instantiability*, not by the engine's own
/// factory lookup — filtering by the table under test would make the assertion
/// true by construction.
#[test]
fn name_sets_are_identical() {
    let engine: BTreeSet<&str> = builtin_names().into_iter().collect();
    let catalog: BTreeSet<&str> = pir::plugins::builtin::KNOWN_BUILTINS
        .iter()
        .map(|row| row.name)
        .filter(|name| pir::plugins::instantiate_builtin(name).is_some())
        .collect();
    assert_eq!(engine, catalog);
}
