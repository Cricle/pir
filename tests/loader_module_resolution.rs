//! Module-resolution tests for the extension loader that used to live inline
//! in `src/extensions/loader.rs` (the hygiene gate keeps `src/` free of test
//! modules): lexical path anchoring, `file:` URL / absolute-path specifiers,
//! on-disk relative imports from an absolute module dir, and the host pi
//! package provisioning contract.
//!
//! Runtime-level cases run against the real `ExtensionRuntime` so the
//! `Resolver`/`Loader` pair is exercised end to end (a seeded module performs
//! the imports); pure helpers are called directly.

use std::path::{Path, PathBuf};

use pir::extensions::loader::{
    absolute_path, discover_extensions, ensure_host_pi_package_root, host_pi_package_dir,
    module_name_to_path, path_specifier_to_local, DependencyKind, DependencyRef,
    HOST_PI_PACKAGE_NAME,
};
use pir::extensions::{ExtensionConfig, ExtensionRuntime};

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "print".to_string(),
        has_ui: false,
        model: None,
        session_id: Some("loader-resolution-test".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// A unique temp directory, created empty.
fn temp_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-loader-resolution-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Runtime whose allowed roots are `dir` and the configured cwd `dir`.
async fn runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

fn json_literal(value: &str) -> String {
    serde_json::to_string(value).expect("json literal")
}

/// Run one seeded probe module (default export required by the extension
/// loader) and return `globalThis.__result` as JSON.
async fn run_probe(rt: &ExtensionRuntime, source: &str) -> serde_json::Value {
    rt.seed_module("pir-ext:loader-probe/probe.js", source)
        .expect("seed probe");
    rt.seeded_load("pir-ext:loader-probe/probe.js")
        .await
        .expect("probe module must load");
    rt.eval_json("JSON.stringify(globalThis.__result)")
        .await
        .expect("probe result")
}

// ---------------------------------------------------------------------------
// Lexical path helpers
// ---------------------------------------------------------------------------

#[test]
fn absolute_path_anchors_relative_spellings_lexically() {
    let cwd = std::env::current_dir().expect("cwd");
    let anchored = absolute_path(Path::new("./ext/../ext/index.js"));
    assert_eq!(anchored, cwd.join("ext").join("index.js"));
    assert!(anchored.is_absolute(), "{anchored:?}");

    // Still lexical: nothing on disk is consulted, so a missing path is
    // anchored just the same.
    let missing = absolute_path(Path::new("no-such-dir/../x.js"));
    assert_eq!(missing, cwd.join("x.js"));
}

#[test]
fn absolute_path_keeps_absolute_spellings_and_resolves_parents() {
    let absolute = if cfg!(windows) {
        PathBuf::from(r"C:\a\b\..\c.js")
    } else {
        PathBuf::from("/a/b/../c.js")
    };
    let anchored = absolute_path(&absolute);
    assert!(anchored.is_absolute(), "{anchored:?}");
    assert!(anchored.ends_with("c.js"), "{anchored:?}");
    assert!(!anchored.to_string_lossy().contains(".."), "{anchored:?}");

    // A `..` above the root is a no-op on both platforms.
    let rooted = if cfg!(windows) {
        PathBuf::from(r"C:\..\x.js")
    } else {
        PathBuf::from("/../x.js")
    };
    let anchored = absolute_path(&rooted);
    assert!(anchored.ends_with("x.js"), "{anchored:?}");
    assert!(!anchored.to_string_lossy().contains(".."), "{anchored:?}");
}

// ---------------------------------------------------------------------------
// `file:` URLs and absolute paths as module specifiers
// ---------------------------------------------------------------------------

#[test]
fn path_specifiers_cover_file_urls_and_absolute_paths() {
    // Non-path spellings keep their own resolution paths.
    assert_eq!(path_specifier_to_local("./x.js"), None);
    assert_eq!(path_specifier_to_local("pi-subagents"), None);
    assert_eq!(path_specifier_to_local("node:path"), None);
    assert_eq!(path_specifier_to_local("@earendil-works/pi-ai"), None);
    // `file:` with no absolute path, or a remote authority, is not local.
    assert_eq!(path_specifier_to_local("file:"), None);
    assert_eq!(path_specifier_to_local("file:relative/x.js"), None);
    assert_eq!(path_specifier_to_local("file://example.com/x.js"), None);
    // Absolute spellings are accepted verbatim.
    assert_eq!(
        path_specifier_to_local("/a/b.js"),
        Some(PathBuf::from("/a/b.js"))
    );
    assert_eq!(
        path_specifier_to_local("C:\\a\\b.js"),
        Some(PathBuf::from("C:\\a\\b.js"))
    );
    assert_eq!(
        path_specifier_to_local("\\\\server\\share\\x.js"),
        Some(PathBuf::from("\\\\server\\share\\x.js"))
    );

    // `file:` URLs: percent-decoded, `localhost` authority stripped, scheme
    // case-insensitive.
    assert_eq!(
        path_specifier_to_local("file:///a/b%20c.js"),
        Some(PathBuf::from("/a/b c.js"))
    );
    assert_eq!(
        path_specifier_to_local("file://localhost/a/b.js"),
        Some(PathBuf::from("/a/b.js"))
    );
    assert_eq!(
        path_specifier_to_local("FILE:///a/b.js"),
        Some(PathBuf::from("/a/b.js"))
    );
    // Windows drive URLs drop the single pathname separator.
    let drive = path_specifier_to_local("file:///C:/x/y.js").expect("drive url");
    if cfg!(windows) {
        assert_eq!(drive, PathBuf::from("C:/x/y.js"));
    } else {
        assert_eq!(drive, PathBuf::from("/C:/x/y.js"));
    }
}

#[test]
fn absolute_path_specifiers_are_supported_dependencies() {
    for specifier in ["file:///C:/x/y.js", "file:///tmp/y.js", "/tmp/y.js"] {
        assert!(
            DependencyRef {
                specifier: specifier.to_string(),
                kind: DependencyKind::DynamicImport,
            }
            .is_supported(),
            "{specifier} must pass the dependency scan"
        );
    }
}

/// The loader resolves `file:` URLs and absolute paths to on-disk modules
/// (the pi-subagents host-SDK / child-session-factory import shape), and the
/// allowed-roots gate still refuses a target outside every registered root.
#[tokio::test(flavor = "current_thread")]
async fn file_url_and_absolute_path_imports_load_from_disk() {
    let dir = temp_dir("file-url-imports");
    let target = dir.join("target.js");
    std::fs::write(
        &target,
        "export const marker = \"file-url-target\";\nexport default 42;\n",
    )
    .expect("target module");
    // Outside the allowed roots: the same directory tree, but never
    // registered (the probe runtime only allows `dir`).
    let outside_dir = temp_dir("file-url-imports-outside");
    let outside = outside_dir.join("outside.js");
    std::fs::write(&outside, "export const marker = \"outside\";\n").expect("outside module");

    let rt = runtime_at(&dir).await;
    let absolute = target.display().to_string().replace('\\', "/");
    let file_url = format!("file:///{}", absolute.trim_start_matches('/'));
    let absolute_url = if cfg!(windows) {
        format!("file:///{}", absolute)
    } else {
        format!("file://{absolute}")
    };
    let outside_absolute = outside.display().to_string().replace('\\', "/");
    let outside_url = format!("file:///{}", outside_absolute.trim_start_matches('/'));

    let probe = format!(
        r#"
export default function () {{}}
globalThis.__result = {{ state: "failed", error: "probe did not run" }};
try {{
  const byFileUrl = await import({file_url});
  const byAbsoluteUrl = await import({absolute_url});
  const byAbsolutePath = await import({absolute});
  let outsideError = null;
  try {{
    await import({outside_url});
  }} catch (error) {{
    outsideError = error && error.message ? error.message : String(error);
  }}
  globalThis.__result = {{
    state: "ok",
    byFileUrl: byFileUrl.marker,
    byAbsoluteUrl: byAbsoluteUrl.marker,
    byAbsolutePathDefault: byAbsolutePath.default,
    outsideError,
  }};
}} catch (error) {{
  globalThis.__result = {{
    state: "threw",
    error: error && error.message ? error.message : String(error),
    stack: error && error.stack ? String(error.stack).slice(0, 800) : null,
  }};
}}
"#,
        file_url = json_literal(&file_url),
        absolute_url = json_literal(&absolute_url),
        absolute = json_literal(&absolute),
        outside_url = json_literal(&outside_url),
    );
    let result = run_probe(&rt, &probe).await;
    eprintln!("[loader] file url probe: {result}");
    assert_eq!(result["state"], "ok", "probe failed: {result}");
    assert_eq!(result["byFileUrl"], "file-url-target");
    assert_eq!(result["byAbsoluteUrl"], "file-url-target");
    assert_eq!(result["byAbsolutePathDefault"], 42);
    let outside_error = result["outsideError"].as_str().expect("outside error");
    assert!(
        outside_error.contains("outside the allowed extension roots")
            || outside_error.contains("cannot find module"),
        "a target outside every allowed root must be refused: {outside_error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside_dir);
}

/// Relative imports resolve against an absolute (drive-rooted on Windows)
/// module dir: the joined target keeps the root instead of degrading to a
/// relative spelling the allowed-roots check could never accept.
#[tokio::test(flavor = "current_thread")]
async fn relative_imports_from_an_absolute_module_dir_keep_their_root() {
    let dir = temp_dir("relative-from-absolute");
    std::fs::create_dir_all(dir.join("pkg/nested")).expect("pkg dir");
    std::fs::write(
        dir.join("pkg/sibling.js"),
        "export const marker = \"sibling\";\n",
    )
    .expect("sibling");
    std::fs::write(
        dir.join("pkg/nested/parent.js"),
        "export const marker = \"parent\";\n",
    )
    .expect("parent");
    let entry = dir.join("pkg/entry.js");
    std::fs::write(
        &entry,
        "import { marker as sibling } from \"./sibling.js\";\n\
         import { marker as parent } from \"./nested/../nested/parent.js\";\n\
         export const markers = [sibling, parent];\n",
    )
    .expect("entry");

    let rt = runtime_at(&dir).await;
    let entry_path = entry.display().to_string().replace('\\', "/");
    let entry_url = format!("file:///{}", entry_path.trim_start_matches('/'));
    let probe = format!(
        r#"
export default function () {{}}
globalThis.__result = {{ state: "failed", error: "probe did not run" }};
try {{
  const mod = await import({entry_url});
  globalThis.__result = {{ state: "ok", markers: mod.markers }};
}} catch (error) {{
  globalThis.__result = {{
    state: "threw",
    error: error && error.message ? error.message : String(error),
  }};
}}
"#,
        entry_url = json_literal(&entry_url),
    );
    let result = run_probe(&rt, &probe).await;
    eprintln!("[loader] relative probe: {result}");
    assert_eq!(result["state"], "ok", "probe failed: {result}");
    assert_eq!(result["markers"][0], "sibling");
    assert_eq!(result["markers"][1], "parent");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Host pi package provisioning
// ---------------------------------------------------------------------------

#[test]
fn host_pi_package_provisioning_is_idempotent_and_discoverable() {
    let dir = temp_dir("host-pi-package");
    let root = ensure_host_pi_package_root(&dir).expect("provision");
    assert_eq!(root, host_pi_package_dir(&dir));

    // Manifest identity + root export, as pi-subagents validates them.
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("package.json")).expect("manifest"),
    )
    .expect("manifest json");
    assert_eq!(manifest["name"], HOST_PI_PACKAGE_NAME);
    assert_eq!(manifest["exports"]["."], "./index.js");
    assert!(root.join("index.js").is_file());

    // Every peer the host package probe requires resolves to a manifest with
    // the exact package name, and the declared subpath files exist.
    for (peer, required_files) in [
        ("@earendil-works/pi-agent-core", vec!["index.js"]),
        ("@earendil-works/pi-tui", vec!["index.js"]),
        (
            "@earendil-works/pi-ai",
            vec!["index.js", "compat.js", "oauth.js", "providers/all.js"],
        ),
        ("typebox", vec!["index.js", "compile.js", "value.js"]),
    ] {
        let peer_dir = root.join("node_modules").join(peer);
        let peer_manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(peer_dir.join("package.json")).expect(peer),
        )
        .expect("peer json");
        assert_eq!(peer_manifest["name"], peer, "peer identity");
        for file in required_files {
            assert!(peer_dir.join(file).is_file(), "{peer}/{file}");
        }
    }

    // Idempotent: a second call rewrites nothing (same mtimes).
    let mtime = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .expect("mtime")
    };
    let entry_mtime = mtime(&root.join("index.js"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    ensure_host_pi_package_root(&dir).expect("provision twice");
    assert_eq!(mtime(&root.join("index.js")), entry_mtime);

    // A stale (user-edited) entry is refreshed back to the re-export.
    std::fs::write(root.join("index.js"), "// stale\n").expect("overwrite entry");
    ensure_host_pi_package_root(&dir).expect("reprovision");
    assert!(std::fs::read_to_string(root.join("index.js"))
        .expect("entry")
        .contains("export * from \"@earendil-works/pi-coding-agent\";"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The runtime publishes the provisioning to extensions: after boot, the host
/// package root env is set, and importing that package's entry by `file:` URL
/// is the same module as the bare host-SDK specifier.
#[tokio::test(flavor = "current_thread")]
async fn runtime_publishes_the_host_pi_package_root() {
    let dir = temp_dir("host-pi-root-env");
    let rt = runtime_at(&dir).await;
    let probe = r#"
export default function () {}
globalThis.__result = { state: "failed", error: "probe did not run" };
try {
  const path = await import("node:path");
  const url = await import("node:url");
  const root = process.env.PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT;
  const bare = await import("@earendil-works/pi-coding-agent");
  const entry = await import(url.pathToFileURL(path.join(root, "index.js")).href);
  globalThis.__result = {
    state: "ok",
    root,
    sameModule: entry.createAgentSession === bare.createAgentSession,
    exports: Object.keys(entry).length,
  };
} catch (error) {
  globalThis.__result = { state: "threw", error: String(error && error.message) };
}
"#;
    let result = run_probe(&rt, probe).await;
    eprintln!("[loader] host root probe: {result}");
    assert_eq!(result["state"], "ok", "probe failed: {result}");
    let root = result["root"].as_str().expect("root");
    assert!(root.ends_with("host-pi-package"), "{root}");
    assert_eq!(result["sameModule"], true);
    assert!(result["exports"].as_u64().unwrap_or(0) > 50, "{result}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[test]
fn discovery_anchors_relative_input_paths() {
    let dir = temp_dir("discovery-anchor");
    std::fs::write(dir.join("index.js"), "export default () => {};\n").expect("entry");

    let cwd = std::env::current_dir().expect("cwd");
    let Some(relative) = relative_spelling(&cwd, &dir) else {
        // Different drive: no relative spelling exists on this host.
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };

    let discovered = discover_extensions(std::slice::from_ref(&relative));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(discovered.errors.is_empty(), "{:?}", discovered.errors);
    let extension = discovered.extensions.first().expect("one entry");
    assert!(
        extension.module_name.starts_with("pir-ext:")
            && module_name_to_path(&extension.module_name)
                .is_some_and(|path| Path::new(path).is_absolute()),
        "relative discovery must produce an absolute module id: {}",
        extension.module_name
    );
    assert!(extension.path.is_absolute(), "{:?}", extension.path);
}

/// `path` spelled relative to `base` (both must share a root/drive).
fn relative_spelling(base: &Path, path: &Path) -> Option<PathBuf> {
    let base: Vec<_> = base.components().collect();
    let target: Vec<_> = path.components().collect();
    let shared = base
        .iter()
        .zip(target.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if shared == 0 {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in shared..base.len() {
        out.push("..");
    }
    for component in &target[shared..] {
        out.push(component.as_os_str());
    }
    Some(out)
}
