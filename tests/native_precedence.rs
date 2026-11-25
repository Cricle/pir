//! Native-vs-JS plugin precedence rules (external test, per project rule: no
//! inline `cfg(test)` modules under `src/`).

use std::path::Path;

use pir::plugins::loader::{
    is_js_package_path_for_tests, native_precedence_for_tests, native_precedence_len_for_tests,
};

#[test]
fn always_on_single_face_rules_ignore_the_opt_in() {
    assert!(native_precedence_for_tests(0, false)); // tool-repair
    assert!(native_precedence_for_tests(1, false)); // phone
}

#[test]
fn full_port_pairs_follow_the_preference_switch() {
    // The pure rule: the flag decides, and the always-on pair ignores it.
    assert!(!native_precedence_for_tests(2, false)); // subagent
    assert!(!native_precedence_for_tests(
        native_precedence_len_for_tests() - 1,
        false
    ));
    for index in 0..native_precedence_len_for_tests() {
        assert!(native_precedence_for_tests(index, true));
    }
}

/// The default is native-first: an installed JS package is dropped when its
/// Rust builtin is on the plan, with no flag and no plugin change.
/// `PIR_PREFER_NATIVE=0` restores upstream first-wins.
#[test]
fn native_pairs_win_by_default_and_can_be_opted_out() {
    use pir::plugins::partition_paths;
    use std::path::PathBuf;

    let paths = vec![
        PathBuf::from("builtin:subagent"),
        PathBuf::from("/home/u/.pi/agent/npm/node_modules/pi-subagents/index.ts"),
    ];
    std::env::remove_var("PIR_PREFER_NATIVE");
    let (builtins, files) = partition_paths(&paths);
    assert_eq!(builtins, vec!["subagent".to_string()]);
    assert!(
        files.is_empty(),
        "the JS package must be dropped by default: {files:?}"
    );

    std::env::set_var("PIR_PREFER_NATIVE", "0");
    let (builtins, files) = partition_paths(&paths);
    assert_eq!(builtins, vec!["subagent".to_string()]);
    assert_eq!(
        files.len(),
        1,
        "PIR_PREFER_NATIVE=0 keeps upstream first-wins: {files:?}"
    );
    std::env::remove_var("PIR_PREFER_NATIVE");
}

#[test]
fn package_path_match_is_case_insensitive_and_component_exact() {
    let path = Path::new("/x/node_modules/PI-SUBAGENTS/index.ts");
    assert!(is_js_package_path_for_tests(path, "pi-subagents"));
    // A partial component must not match.
    assert!(!is_js_package_path_for_tests(path, "pi-subagent"));
}
