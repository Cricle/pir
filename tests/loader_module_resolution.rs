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
    module_name_for, module_name_to_path, normalize_relative_for_test, path_specifier_to_local,
    split_specifier_suffix_for_test, DependencyKind, DependencyRef, HOST_PI_PACKAGE_NAME,
};
use pir::extensions::{ExtensionConfig, ExtensionRuntime};

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    test_config_at_in(cwd, None)
}

/// [`test_config_at`] with the agent directory the runtime's boot-time
/// pi-package provisioning publishes into (`<agent_dir>/pir/host-pi-package`).
/// A test that reads that root passes its own directory instead of pinning
/// `PIR_AGENT_DIR` in the process environment.
fn test_config_at_in(cwd: &Path, agent_dir: Option<&Path>) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "print".to_string(),
        has_ui: false,
        model: None,
        session_id: Some("loader-resolution-test".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
        allow_process_exit: false,
        agent_dir: agent_dir.map(Path::to_path_buf),
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
    runtime_at_in(dir, None).await
}

/// [`runtime_at`] with an explicit agent directory for the boot-time
/// pi-package provisioning.
async fn runtime_at_in(dir: &Path, agent_dir: Option<&Path>) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at_in(dir, agent_dir))
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
///
/// `runtime.rs` provisions `<agent_dir>/pir/host-pi-package` from the config's
/// agent directory; the runtime below is given a scratch one explicitly, so
/// the root this test reads belongs to THIS scenario and never another
/// suite's `PIR_AGENT_DIR`.
#[tokio::test(flavor = "current_thread")]
async fn runtime_publishes_the_host_pi_package_root() {
    let agent_dir = crate::common::scratch_agent_dir("loader-host-pi-root", None);
    let dir = temp_dir("host-pi-root-env");
    let rt = runtime_at_in(&dir, Some(agent_dir.path())).await;
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

// ---------------------------------------------------------------------------
// Parent-relative ESM resolution: ../ is valid within an explicitly allowed
// package root, but a loaded importer cannot extend that boundary.
// ---------------------------------------------------------------------------

#[test]
fn relative_join_resolves_parents_and_keeps_the_root() {
    // `..` climbs one component; a run of them stops at the root instead of
    // deleting the root (the old string splitter dropped the component before
    // `..`).
    assert_eq!(
        normalize_relative_for_test("C:/root/pkg/parallel", "../common/index.mjs"),
        "C:/root/pkg/common/index.mjs"
    );
    assert_eq!(
        normalize_relative_for_test("C:/root/pkg/parallel/deep", "../../../shared/x.mjs"),
        "C:/root/shared/x.mjs"
    );
    assert_eq!(
        normalize_relative_for_test("/posix/root/pkg/parallel", "../common/index.mjs"),
        "/posix/root/pkg/common/index.mjs"
    );
    assert_eq!(
        normalize_relative_for_test("/posix/root", "../../x.mjs"),
        "/x.mjs"
    );

    #[cfg(windows)]
    {
        // Drive roots: `..` above `C:/` is a no-op, not a drive-relative path.
        assert_eq!(
            normalize_relative_for_test("C:/root", "../../x.mjs"),
            "C:/x.mjs"
        );
        assert_eq!(normalize_relative_for_test("C:/", ".."), "C:/");
        assert_eq!(
            normalize_relative_for_test("C:/root/pkg/a/b", "../../../../x.mjs"),
            "C:/x.mjs"
        );
        // UNC share: both leading separators and the server/share prefix stay.
        assert_eq!(
            normalize_relative_for_test("//server/share/pkg", "../x.mjs"),
            "//server/share/x.mjs"
        );
        assert_eq!(
            normalize_relative_for_test("//server/share/pkg/a", "../../x.mjs"),
            "//server/share/x.mjs"
        );
    }
}

#[test]
fn relative_join_decodes_percent_escapes_and_drops_dot_segments() {
    assert_eq!(
        normalize_relative_for_test("C:/root/common", "./space%20name.mjs"),
        "C:/root/common/space name.mjs"
    );
    assert_eq!(
        normalize_relative_for_test("C:/root/pkg", "a/./b/../c.mjs"),
        "C:/root/pkg/a/c.mjs"
    );
    // A malformed escape stays literal (Node's URL parser behavior).
    assert_eq!(
        normalize_relative_for_test("C:/root", "./100%zz.mjs"),
        "C:/root/100%zz.mjs"
    );
}

#[test]
fn specifier_suffixes_are_split_off_the_filesystem_path() {
    assert_eq!(
        split_specifier_suffix_for_test("./x.mjs?q=1#frag"),
        ("./x.mjs".to_string(), "?q=1#frag".to_string())
    );
    assert_eq!(
        split_specifier_suffix_for_test("../x.mjs#frag"),
        ("../x.mjs".to_string(), "#frag".to_string())
    );
    assert_eq!(
        split_specifier_suffix_for_test("./x.mjs"),
        ("./x.mjs".to_string(), String::new())
    );
    // `%23` is a filename character, not a fragment delimiter.
    assert_eq!(
        split_specifier_suffix_for_test("./a%23b.mjs"),
        ("./a%23b.mjs".to_string(), String::new())
    );
}

/// Best-effort file symlink; unsupported platforms (or a Windows host without
/// the privilege) report `false` and the caller drops the identity assertion.
fn try_relative_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, link);
        false
    }
}

fn relative_entry_file_url(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    let encoded = text
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('?', "%3F")
        .replace('#', "%23");
    format!("file:///{}", encoded.trim_start_matches('/'))
}

/// A `.mjs` entry below `test/parallel/` importing across directory boundaries:
///
/// - `../common/index.mjs` (parent relative; the audit defect),
/// - `../../deep.mjs` (two levels up, still within the package root),
/// - `../common/space%20%23name.mjs` (percent-encoded file name),
/// - `../dir/` (directory -> index.mjs),
/// - `../common/index.mjs?mode=1` (query = separate module instance),
/// - `../common/./index.mjs` (same canonical module, not re-evaluated),
/// - `../common/data.json` with `{ type: "json" }`,
/// - `{ default as namedDefault, named }` from a sibling module,
/// - top-level await in the imported module and in the entry,
/// - a dynamic `import()` of a directory index,
/// - an extensionless `import('../deep')` that MUST fail (no `.mjs` appended).
///
/// The explicitly registered package root contains `test/` and `deep.mjs`.
#[tokio::test(flavor = "current_thread")]
async fn relative_esm_chain_resolves_across_directories() {
    let root = temp_dir("relative-chain");
    let test = root.join("test");
    std::fs::create_dir_all(test.join("parallel")).expect("parallel");
    std::fs::create_dir_all(test.join("common")).expect("common");
    std::fs::create_dir_all(test.join("dir")).expect("dir");

    std::fs::write(root.join("deep.mjs"), "export const deep = \"deep-ok\";\n").expect("deep");
    std::fs::write(
        test.join("common/index.mjs"),
        "\
globalThis.__relative_eval_count = (globalThis.__relative_eval_count || 0) + 1;\n\
export const value = \"parent-ok\";\n\
export const late = await Promise.resolve(\"tla-ok\");\n",
    )
    .expect("common index");
    std::fs::write(
        test.join("common/space #name.mjs"),
        "export const space = \"space-ok\";\n",
    )
    .expect("space");
    std::fs::write(
        test.join("common/named.mjs"),
        "export default \"default-ok\";\nexport const named = \"named-ok\";\n",
    )
    .expect("named");
    std::fs::write(test.join("common/data.json"), "{\"hello\":\"json-ok\"}\n").expect("json");
    std::fs::write(
        test.join("dir/index.mjs"),
        "export const idx = \"index-ok\";\n",
    )
    .expect("dir index");
    std::fs::write(
        test.join("dir/boom.mjs"),
        "export function boom() { throw new Error(\"boom-here\"); }\n",
    )
    .expect("dir boom");

    // Symlink support is optional on Windows (privilege); only add the
    // identity assertion when the link could actually be created.
    let mut symlink_import = String::new();
    let mut symlink_field = String::new();
    let linked = try_relative_symlink(
        &test.join("common/index.mjs"),
        &test.join("common/link.mjs"),
    );
    if linked {
        symlink_import = "import * as linkedModule from \"../common/link.mjs\";\n".to_string();
        symlink_field = ", symlinkSame: linkedModule === first".to_string();
    }

    let mut case_import = String::new();
    let mut case_field = String::new();
    if cfg!(windows) {
        // Windows file lookup is case-insensitive; module identity is the
        // canonical path, so a differently-cased spelling is ONE module.
        case_import = "import * as cased from \"../COMMON/INDEX.MJS\";\n".to_string();
        case_field = ", caseSame: cased === first".to_string();
    }

    let entry = format!(
        "\
import * as first from \"../common/index.mjs\";\n\
import * as again from \"../common/./index.mjs\";\n\
import {{ value, late }} from \"../common/index.mjs\";\n\
import {{ deep }} from \"../../deep.mjs\";\n\
import {{ space }} from \"../common/space%20%23name.mjs\";\n\
import {{ idx }} from \"../dir/\";\n\
import * as queried from \"../common/index.mjs?mode=1\";\n\
import data from \"../common/data.json\" with {{ type: \"json\" }};\n\
import {{ default as namedDefault, named }} from \"../common/named.mjs\";\n\
{symlink_import}{case_import}\
const dyn = await import(\"../dir/index.mjs\");\n\
let missingError = \"\";\n\
try {{\n\
  await import(\"../deep\");\n\
}} catch (error) {{\n\
  missingError = String((error && error.message) || error);\n\
}}\n\
let dynamicStack = \"\";\n\
try {{\n\
  const bad = await import(\"../dir/boom.mjs\");\n\
  bad.boom();\n\
}} catch (error) {{\n\
  dynamicStack = String((error && error.stack) || error);\n\
}}\n\
const builtinPrefixSame = (await import(\"node:path\")) === (await import(\"path\"));\n\
export const result = {{\n\
  value, late, deep, space, idx, dyn: dyn.idx,\n\
  json: data.hello, namedDefault, named,\n\
  sameCanonical: first === again,\n\
  queryDistinct: first !== queried && queried.value === first.value,\n\
  evalCount: globalThis.__relative_eval_count,\n\
  metaUrl: import.meta.url, metaFilename: import.meta.filename, metaDirname: import.meta.dirname,\n\
  dynamicStack,\n\
  missingError, builtinPrefixSame{symlink_field}{case_field}\n\
}};\n",
        symlink_import = symlink_import,
        case_import = case_import,
        symlink_field = symlink_field,
        case_field = case_field,
    );
    let entry_path = test.join("parallel/entry.mjs");
    std::fs::write(&entry_path, &entry).expect("entry");

    let rt = ExtensionRuntime::new(test_config_at(&test.join("parallel")))
        .await
        .expect("runtime construction");
    rt.allow_root(root.clone());

    let entry_url = relative_entry_file_url(&entry_path);
    let probe = format!(
        "\
export default function () {{}}\n\
globalThis.__result = {{ state: \"failed\" }};\n\
try {{\n\
  const m = await import({entry_url});\n\
  globalThis.__result = Object.assign({{ state: \"ok\" }}, m.result);\n\
}} catch (error) {{\n\
  globalThis.__result = {{\n\
    state: \"threw\",\n\
    error: String((error && error.message) || error),\n\
    stack: error && error.stack ? String(error.stack).slice(0, 800) : null,\n\
  }};\n\
}}\n",
        entry_url = json_literal(&entry_url),
    );
    let result = run_probe(&rt, &probe).await;
    eprintln!("[loader] relative chain: {result}");
    assert_eq!(result["state"], "ok", "chain failed: {result}");

    assert_eq!(result["value"], "parent-ok", "{result}");
    assert_eq!(
        result["late"], "tla-ok",
        "top-level await in an import: {result}"
    );
    assert_eq!(result["deep"], "deep-ok", "two-level parent: {result}");
    assert_eq!(
        result["space"], "space-ok",
        "percent-decoded name: {result}"
    );
    assert_eq!(result["idx"], "index-ok", "directory import: {result}");
    assert_eq!(result["dyn"], "index-ok", "dynamic import: {result}");
    assert_eq!(
        result["json"], "json-ok",
        "JSON import attributes: {result}"
    );
    assert_eq!(result["namedDefault"], "default-ok", "{result}");
    assert_eq!(result["named"], "named-ok", "{result}");
    assert_eq!(
        result["sameCanonical"], true,
        "./x and ././x must be one module: {result}"
    );
    assert_eq!(
        result["queryDistinct"], true,
        "?query is a distinct module instance: {result}"
    );
    assert_eq!(
        result["evalCount"], 2,
        "one evaluation for the file, one for its ?mode=1 URL: {result}"
    );
    assert_eq!(
        result["builtinPrefixSame"], true,
        "node:x and x are one builtin module: {result}"
    );
    let missing = result["missingError"].as_str().unwrap_or_default();
    assert!(
        missing.contains("Cannot find module"),
        "an extensionless import must not get `.mjs` appended: {missing}"
    );
    // `import.meta` carries the module's URL and native filename/dirname; a
    // dynamically imported module's stack carries its own filename (the
    // `strip_debug` regression guard).
    let meta_url = result["metaUrl"].as_str().unwrap_or_default();
    assert!(
        meta_url.starts_with("file:///") && meta_url.ends_with("/entry.mjs"),
        "import.meta.url must be the module's file URL: {meta_url}"
    );
    let meta_filename = result["metaFilename"].as_str().unwrap_or_default();
    assert!(
        meta_filename.ends_with("entry.mjs") && !meta_filename.contains("pir-ext:"),
        "import.meta.filename must be a native path: {meta_filename}"
    );
    let meta_dirname = result["metaDirname"].as_str().unwrap_or_default();
    assert!(
        meta_dirname.ends_with("parallel"),
        "import.meta.dirname must be the module directory: {meta_dirname}"
    );
    let dynamic_stack = result["dynamicStack"].as_str().unwrap_or_default();
    assert!(
        dynamic_stack.contains("boom.mjs") && dynamic_stack.contains("boom-here"),
        "dynamic import() must keep the module filename in the stack: {dynamic_stack}"
    );
    if linked {
        assert_eq!(
            result["symlinkSame"], true,
            "a symlink and its target are one module (realpath): {result}"
        );
    }
    if cfg!(windows) {
        assert_eq!(
            result["caseSame"], true,
            "case-variant spellings are one module: {result}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn relative_imports_do_not_extend_the_allowed_roots() {
    let root = temp_dir("relative-boundary");
    let pkg = root.join("pkg");
    let sibling = root.join("pkg-sibling");
    std::fs::create_dir(&pkg).expect("package directory");
    std::fs::create_dir(&sibling).expect("neighbor directory");
    let target = sibling.join("neighbor.mjs");
    std::fs::write(&target, "export default 'neighbor';").expect("neighbor module");
    let entry = pkg.join("entry.mjs");
    std::fs::write(
        &entry,
        r#"
export async function probe(specifier) {
  try { return (await import(specifier)).default; }
  catch (error) { return String(error.message); }
}
"#,
    )
    .expect("entry module");
    let rt = runtime_at(&pkg).await;
    let linked = try_relative_symlink(&target, &pkg.join("alias.mjs"));
    let mut specifiers = vec![
        "../pkg-sibling/neighbor.mjs".to_string(),
        target.to_string_lossy().replace('\\', "/"),
        relative_entry_file_url(&target),
        module_name_for(&target),
    ];
    if linked {
        let link = pkg.join("alias.mjs");
        specifiers.extend([
            "./alias.mjs".to_string(),
            link.to_string_lossy().replace('\\', "/"),
            relative_entry_file_url(&link),
            module_name_for(&link),
        ]);
    }
    let source = format!(
        r#"
export default function () {{}}
const {{ probe }} = await import({});
globalThis.__result = [];
for (const specifier of {}) {{ globalThis.__result.push(await probe(specifier)); }}
"#,
        json_literal(&relative_entry_file_url(&entry)),
        serde_json::to_string(&specifiers).expect("specifier list"),
    );
    let result = run_probe(&rt, &source).await;
    let errors = result.as_array().expect("one error per specifier");
    assert_eq!(errors.len(), specifiers.len());
    for (specifier, error) in specifiers.iter().zip(errors) {
        assert!(
            error
                .as_str()
                .is_some_and(|error| error.contains("outside the allowed extension roots")),
            "{specifier}: {error}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn bare_packages_grant_only_their_contained_package_directory() {
    let root = temp_dir("bare-package-boundary");
    let script = root.join("script");
    let package = root.join("node_modules/loader-fixture");
    std::fs::create_dir(&script).expect("script directory");
    std::fs::create_dir_all(&package).expect("package directory");
    std::fs::write(
        package.join("package.json"),
        r#"{"type":"module","exports":"./index.mjs"}"#,
    )
    .expect("package manifest");
    std::fs::write(package.join("sibling.mjs"), "export const marker = 42;")
        .expect("package sibling");
    std::fs::write(
        package.join("index.mjs"),
        "export { marker } from './sibling.mjs'; export const url = import.meta.url;",
    )
    .expect("package entry");
    std::fs::write(root.join("neighbor.mjs"), "export default 'neighbor';")
        .expect("neighbor module");
    let other_package = root.join("node_modules/loader-fixture-other");
    std::fs::create_dir(&other_package).expect("other package directory");
    std::fs::write(
        other_package.join("package.json"),
        r#"{"main":"../../neighbor.mjs"}"#,
    )
    .expect("other package manifest");

    let entry = script.join("entry.mjs");
    std::fs::write(
        &entry,
        r#"
const bare = await import('loader-fixture?case=1#fixture');
const byUrl = await import(bare.url);
const distinct = await import('loader-fixture?case=1#other');
let otherError = null;
try { await import('loader-fixture-other'); }
catch (error) { otherError = String(error.message); }
export const result = { marker: bare.marker, same: bare === byUrl, distinct: bare !== distinct, otherError };
"#,
    )
    .expect("script entry");
    let rt = runtime_at(&script).await;
    let result = run_probe(
        &rt,
        &format!(
            "export default function () {{}}\nglobalThis.__result = (await import({})).result;",
            json_literal(&relative_entry_file_url(&entry)),
        ),
    )
    .await;
    assert_eq!(result["marker"], 42, "{result}");
    assert_eq!(result["same"], true, "{result}");
    assert_eq!(result["distinct"], true, "{result}");
    assert!(
        result["otherError"]
            .as_str()
            .is_some_and(|error| error.contains("outside the resolved npm package root")),
        "{result}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn seeded_disk_sources_pass_the_same_check_at_load_time() {
    use pir::extensions::loader::{AllowedRoots, CachedSource, ExtensionModuleLoader, SourceCache};
    use rquickjs::loader::Loader;

    let root = temp_dir("seeded-source-containment");
    let allowed = root.join("allowed");
    std::fs::create_dir(&allowed).expect("allowed directory");
    let allowed = std::fs::canonicalize(allowed).expect("real allowed directory");
    let inside = allowed.join("inside.mjs");
    let missing = allowed.join("missing.mjs");
    let outside = root.join("outside.mjs");
    for file in [&inside, &outside] {
        std::fs::write(file, "export default 'on disk';").expect("module fixture");
    }
    let outside = std::fs::canonicalize(outside).expect("real outside file");
    let cache = SourceCache::default();
    let roots = AllowedRoots::default();
    roots.lock().expect("roots").push(allowed);
    for file in [&inside, &outside, &missing] {
        cache.lock().expect("seeded sources").insert(
            module_name_for(file),
            CachedSource {
                js: "export default 'seeded';".into(),
                key: None,
            },
        );
    }
    let mut loader = ExtensionModuleLoader::new(cache, roots);
    let runtime = rquickjs::Runtime::new().expect("runtime");
    let context = rquickjs::Context::full(&runtime).expect("context");
    context.with(|ctx| {
        assert!(loader.load(&ctx, &module_name_for(&inside), None).is_ok());
        assert!(loader.load(&ctx, &module_name_for(&outside), None).is_err());
        assert!(loader.load(&ctx, &module_name_for(&missing), None).is_err());
    });
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn discovered_entries_need_an_explicit_containing_root() {
    let root = temp_dir("discovered-entry-boundary");
    let package = root.join("pkg");
    std::fs::create_dir(&package).expect("package directory");
    std::fs::write(
        package.join("package.json"),
        r#"{"pi":{"extensions":["../outside.mjs"]}}"#,
    )
    .expect("package manifest");
    std::fs::write(
        root.join("outside.mjs"),
        "export default function () { globalThis.__unexpectedEntry = true; }",
    )
    .expect("outside fixture");

    let rt = runtime_at(&package).await;
    let result = rt.load(&[package]).await.expect("load result");
    assert!(result.loaded.is_empty(), "{:?}", result.loaded);
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
    assert!(result.errors[0]
        .error
        .contains("outside the allowed extension roots"));
    let _ = std::fs::remove_dir_all(&root);
}
