//! Unit-level tests for the `pir-node` crate — kept in `tests/`, per project
//! rule (no inline `cfg(test)` under `src/`).

use pir::pir_node::{bridge, NodeError};
use serde_json::json;

#[test]
fn error_envelope_matches_legacy_shape() {
    let err = NodeError::new("ENOENT", "boom");
    let value = err.to_json_value();
    assert_eq!(value["ok"], false);
    assert_eq!(value["code"], "ENOENT");
    assert_eq!(value["message"], "boom");
    assert!(value.get("errno").is_some());
    assert!(value.get("path").is_none());
}

#[test]
fn denied_carries_syscall_path_and_errno() {
    let err = NodeError::denied("open", "/etc/passwd");
    let value = err.to_json_value();
    assert_eq!(value["code"], "EACCES");
    assert_eq!(value["syscall"], "open");
    assert_eq!(value["path"], "/etc/passwd");
    assert!(value.get("errno").is_some());
}

#[test]
fn normalize_collapses_dot_and_dotdot() {
    use std::path::PathBuf;
    assert_eq!(
        bridge::normalize_for_check(std::path::Path::new("/a/b/../c/./d")),
        PathBuf::from("/a/c/d")
    );
}

#[test]
fn path_within_respects_roots() {
    use pir::pir_node::bridge;
    use std::path::PathBuf;

    // Real, existing root so the canonicalizing normalizer behaves exactly as
    // it does for the sandbox at runtime (a non-existent Windows path would
    // canonicalize "/project" against the current drive root instead).
    let root = std::env::temp_dir().join(format!("pir-node-path-within-{}", std::process::id()));
    std::fs::create_dir_all(root.join("a")).expect("create root");
    let elsewhere = std::env::temp_dir().join(format!("pir-node-elsewhere-{}", std::process::id()));

    let roots = vec![root.clone()];
    assert!(bridge::path_within(&root.join("a").join("b"), &roots));
    assert!(!bridge::path_within(&elsewhere, &roots));
    assert!(!bridge::path_within(&PathBuf::from(&root).join("..").join("sibling"), &roots));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn system_module_dispatch_works() {
    use pir::pir_node::{NullContext, NullHooks, NodeRuntime, SessionConfig};
    use std::sync::{Arc, Mutex};

    let runtime = NodeRuntime::new(
        Arc::new(Mutex::new(Vec::new())),
        SessionConfig::default(),
        Arc::new(NullContext),
        Arc::new(NullHooks),
    );
    let result = runtime.invoke("os", "homedir", &json!({})).unwrap();
    assert!(result.is_string());
    let cpus = runtime.invoke("os", "cpus_count", &json!({})).unwrap();
    assert!(cpus.as_u64().unwrap_or(0) >= 1);
    let env = runtime.invoke("process", "env", &json!({})).unwrap();
    assert!(env.is_object());
}

#[test]
fn buffer_compare_ranges_is_lexicographic() {
    use pir::pir_node::modules::buffer::compare_ranges;

    assert_eq!(compare_ranges(b"abc", b"abc"), 0);
    assert_eq!(compare_ranges(b"abc", b"abd"), -1);
    assert_eq!(compare_ranges(b"abd", b"abc"), 1);
    // Equal prefix: the shorter range sorts first.
    assert_eq!(compare_ranges(b"abc", b"abcd"), -1);
    assert_eq!(compare_ranges(b"abcd", b"abc"), 1);
    assert_eq!(compare_ranges(&[], &[]), 0);
    assert_eq!(compare_ranges(&[0xff], &[0x00]), 1);
}

#[test]
fn buffer_swap_in_place_reverses_each_group() {
    use pir::pir_node::modules::buffer::swap_in_place;

    let mut two = [0x01u8, 0x02, 0x03, 0x04];
    swap_in_place(&mut two, 2);
    assert_eq!(two, [0x02, 0x01, 0x04, 0x03]);

    let mut four = [1u8, 2, 3, 4];
    swap_in_place(&mut four, 4);
    assert_eq!(four, [4, 3, 2, 1]);

    let mut eight = [1u8, 2, 3, 4, 5, 6, 7, 8];
    swap_in_place(&mut eight, 8);
    assert_eq!(eight, [8, 7, 6, 5, 4, 3, 2, 1]);
}

#[test]
fn path_kernels_match_node_semantics() {
    use pir::pir_node::modules::path::{self, Flavor};

    // win32: drive roots, UNC, device roots, reserved names and the trailing
    // separator rule (verified byte-for-byte against Node v24).
    assert_eq!(path::normalize(Flavor::Win32, "C:\\a\\..\\b\\\\c\\"), "C:\\b\\c\\");
    assert_eq!(
        path::normalize(Flavor::Win32, "\\\\server\\share\\x\\..\\y"),
        "\\\\server\\share\\y"
    );
    assert_eq!(path::normalize(Flavor::Win32, "C:\\CON"), "C:\\CON");
    assert_eq!(
        path::join(Flavor::Win32, &["//server", "share"]),
        "\\\\server\\share\\"
    );
    assert_eq!(
        path::relative(Flavor::Win32, "C:\\a\\b", "C:\\a\\c", "C:\\"),
        "..\\c"
    );

    // posix: the same operations under forward slashes.
    assert_eq!(path::normalize(Flavor::Posix, "/a//b/../c/"), "/a/c/");
    assert_eq!(path::normalize(Flavor::Posix, "../a"), "../a");
    assert_eq!(
        path::join(Flavor::Posix, &["a/", "/b", "//c"]),
        "a/b/c"
    );
    assert_eq!(
        path::relative(Flavor::Posix, "/a/b/c", "/a/b/d/e", "/"),
        "../d/e"
    );
    assert_eq!(
        path::resolve(Flavor::Posix, &["/a", "b", "..", "c"], "/"),
        "/a/c"
    );
}
