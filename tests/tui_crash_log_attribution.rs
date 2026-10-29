//! Crash-log extension attribution (audit §7 #20).
//!
//! Upstream `core/crash-log.ts` records the loaded extensions whose source
//! files appear in a crash stack (`findExtensionStackMatches`), so the crash
//! record points at the culprit. pir keeps the persisted `CrashRecord` shape
//! unchanged and appends the matched labels to the recorded stack.

use pir::coding_agent::crash_log::{
    attach_extension_attribution, clear_loaded_extensions, find_extension_stack_matches,
    loaded_extensions, register_loaded_extensions, ExtensionStackMetadata,
};

fn meta(path: &str) -> ExtensionStackMetadata {
    ExtensionStackMetadata::from_path(path)
}

#[test]
fn exact_file_matches_require_a_path_boundary() {
    let extensions = vec![meta("C:\\work\\extensions\\hello.ts")];
    let stack = "at C:/work/extensions/hello.ts:12:3\n    at pir::agent::run";
    assert_eq!(
        find_extension_stack_matches(stack, &extensions),
        vec!["C:\\work\\extensions\\hello.ts".to_string()]
    );

    // A same-prefix file (`hello.tsx`) is NOT the extension entry.
    let stack = "at C:/work/extensions/hello.tsx:12:3";
    assert!(find_extension_stack_matches(stack, &extensions).is_empty());

    // A sibling frame below the entry file also does not match the file
    // itself (upstream exact match semantics) ...
    let stack = "at C:/work/extensions/hello.ts/helper.js:1:1";
    assert!(find_extension_stack_matches(stack, &extensions).is_empty());

    // ... but a backslash-spelled stack matches the normalized entry path.
    let stack = "at C:\\work\\extensions\\hello.ts:20:1";
    assert_eq!(find_extension_stack_matches(stack, &extensions).len(), 1);
}

#[test]
fn directory_entries_and_package_roots_match_descendant_frames() {
    let extensions = vec![meta("/home/u/.pi/extensions/plugin/index.ts")];
    let stack = "at /home/u/.pi/extensions/plugin/lib/helper.js:3:1";
    assert_eq!(
        find_extension_stack_matches(stack, &extensions),
        vec!["/home/u/.pi/extensions/plugin/index.ts".to_string()]
    );

    // A package root matches any frame below it, even without index.*.
    let package = ExtensionStackMetadata::new("/home/u/.pi/packages/widget/ext.js", "widget")
        .with_base_dir("/home/u/.pi/packages/widget");
    let stack = "at /home/u/.pi/packages/widget/dist/inner.js:1:1";
    assert_eq!(
        find_extension_stack_matches(stack, &[package]),
        vec!["widget".to_string()]
    );

    // A non-index entry is file-exact and does not claim its directory.
    let plain = vec![meta("/home/u/.pi/extensions/solo.ts")];
    assert!(
        find_extension_stack_matches("at /home/u/.pi/extensions/other.ts:1:1", &plain).is_empty()
    );
}

#[test]
fn windows_paths_compare_case_insensitively_and_synthetic_paths_never_match() {
    let extensions = vec![meta("C:\\Users\\Dev\\.pi\\extensions\\Hi.TS")];
    let stack = "at c:/users/dev/.pi/extensions/hi.ts:1:1";
    assert_eq!(find_extension_stack_matches(stack, &extensions).len(), 1);

    let synthetic = vec![meta("pir-ext:builtin/todo")];
    assert!(find_extension_stack_matches("at pir-ext:builtin/todo:1:1", &synthetic).is_empty());
    let synthetic = vec![meta("builtin:todo")];
    assert!(find_extension_stack_matches("at builtin:todo:1:1", &synthetic).is_empty());
}

#[test]
fn percent_encoded_stack_paths_are_decoded() {
    let extensions = vec![meta("/home/u/.pi/extensions/my ext/index.js")];
    let stack = "at /home/u/.pi/extensions/my%20ext/lib/x.js:1:1";
    assert_eq!(
        find_extension_stack_matches(stack, &extensions),
        vec!["/home/u/.pi/extensions/my ext/index.js".to_string()]
    );
}

#[test]
fn duplicate_labels_are_reported_once() {
    let extensions = vec![
        ExtensionStackMetadata::new("/pkg/a/index.js", "pkg").with_base_dir("/pkg/a"),
        ExtensionStackMetadata::new("/pkg/b/index.js", "pkg").with_base_dir("/pkg/b"),
    ];
    let stack = "at /pkg/a/x.js:1:1\nat /pkg/b/y.js:2:2";
    assert_eq!(
        find_extension_stack_matches(stack, &extensions),
        vec!["pkg".to_string()]
    );
}

#[test]
fn attribution_appends_a_named_section_to_the_stack() {
    assert_eq!(attach_extension_attribution("stack", &[]), "stack");
    let annotated = attach_extension_attribution(
        "at src/main.rs:1:1\nframe",
        &["hello".to_string(), "widget".to_string()],
    );
    assert!(annotated.starts_with("at src/main.rs:1:1\nframe\n"));
    assert!(annotated.contains("\nloaded extensions in this stack:\n  - hello\n  - widget\n"));
}

#[test]
fn loaded_extension_registry_roundtrips() {
    clear_loaded_extensions();
    assert!(loaded_extensions().is_empty());
    register_loaded_extensions(vec![
        ExtensionStackMetadata::new("/x/a.ts", "a"),
        ExtensionStackMetadata::new("/x/b.ts", "b"),
    ]);
    let loaded = loaded_extensions();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].label, "a");
    clear_loaded_extensions();
    assert!(loaded_extensions().is_empty());
}

#[cfg(feature = "extensions")]
#[test]
fn discovering_paths_registers_the_entry_points_for_attribution() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-crash-ext-{stamp}"));
    std::fs::create_dir_all(dir.join("plugin")).expect("mkdir plugin");
    std::fs::write(dir.join("hello.ts"), "export default {};\n").expect("write hello");
    std::fs::write(
        dir.join("plugin").join("index.js"),
        "module.exports = {};\n",
    )
    .expect("write plugin");
    std::fs::create_dir_all(dir.join("plugin").join("lib")).expect("mkdir lib");
    std::fs::write(
        dir.join("plugin").join("lib").join("helper.js"),
        "module.exports = 1;\n",
    )
    .expect("write helper");

    let metadata =
        pir::coding_agent::crash_log::register_extensions_from_paths(std::slice::from_ref(&dir));
    let labels: Vec<String> = metadata
        .iter()
        .map(|meta| meta.label.replace('\\', "/"))
        .collect();
    assert!(
        labels.iter().any(|label| label.ends_with("hello.ts")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.ends_with("plugin/index.js")),
        "{labels:?}"
    );

    // The directory entry's nested frames are attributed to that extension.
    let stack = format!(
        "at {}\n    at pir::agent::run",
        dir.join("plugin").join("lib").join("helper.js").display()
    );
    let matched = find_extension_stack_matches(
        &stack.replace('\\', "/"),
        &pir::coding_agent::crash_log::loaded_extensions(),
    );
    assert!(
        matched.iter().any(|label| label.contains("plugin")),
        "{matched:?}"
    );

    clear_loaded_extensions();
    let _ = std::fs::remove_dir_all(&dir);
}
