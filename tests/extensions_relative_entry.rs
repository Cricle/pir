//! Regression: an extension entry named by a RELATIVE path.
//!
//! `pir -e ./pi-rel-ext` (and any relative settings entry) used to fail with
//! `Error loading module 'pir-ext:...': module source not found`: discovery
//! built the synthetic module id from the relative spelling, `module_dir`
//! then dropped the leading `./` in the relative-import join, and the
//! on-disk load check compared that relative spelling against the
//! (component-wise) allowed roots — so both relative imports and bare
//! `node_modules` specifiers were rejected while the identical absolute path
//! loaded. Discovery now anchors every entry path, and the allowed-roots
//! check anchors both sides, so a relative `-e` behaves like its absolute
//! spelling: same module id, same resolution, same globals.
//!
//! Own file (not `suites/`): the path/module family work stream owns the
//! loader, and the wider fixture harness in `suites/plugins_unified.rs` stays
//! another stream's conflict scope.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};

fn write_fixture(test_name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-rel-entry-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("pi-rel-ext");
    std::fs::create_dir_all(root.join("lib")).expect("lib dir");
    std::fs::create_dir_all(root.join("node_modules").join("rel-dep")).expect("dep dir");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "pi-rel-ext", "version": "1.0.0", "pi": { "extensions": ["./index.js"] } }"#,
    )
    .expect("manifest");
    // Relative import (on-disk sibling) + bare specifier (`node_modules`
    // walk-up): both go through the loader's on-disk path, which is what the
    // relative entry spelling used to break.
    std::fs::write(
        root.join("index.js"),
        "import { tag } from \"./lib/util.js\";\n\
         import dep from \"rel-dep\";\n\
         export default (pi) => {\n\
           globalThis.__rel_ext_loaded = tag + \"/\" + dep.dep;\n\
         };\n",
    )
    .expect("entry");
    std::fs::write(
        root.join("lib").join("util.js"),
        "export const tag = \"rel-ok\";\n",
    )
    .expect("util");
    std::fs::write(
        root.join("node_modules")
            .join("rel-dep")
            .join("package.json"),
        r#"{ "name": "rel-dep", "version": "1.0.0", "main": "index.js" }"#,
    )
    .expect("dep manifest");
    std::fs::write(
        root.join("node_modules").join("rel-dep").join("index.js"),
        "module.exports = { dep: \"dep-ok\" };\n",
    )
    .expect("dep entry");
    root
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
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

#[tokio::test(flavor = "current_thread")]
async fn relative_entry_path_loads_like_its_absolute_spelling() {
    let root = write_fixture("relative-entry");
    let cwd = std::env::current_dir().expect("cwd");
    let relative = relative_spelling(&cwd, &root);
    if relative.is_none() {
        // Cross-drive (Windows): no relative spelling exists on this host, so
        // only the relative-vs-absolute comparison is skipped. The absolute
        // load below still runs and proves the entry and its imports load.
        eprintln!("skipping the relative half: no relative spelling from {cwd:?} to {root:?}");
    }

    let relative_rt = match &relative {
        Some(relative) => {
            assert!(!relative.is_absolute(), "{relative:?}");
            let rt = ExtensionRuntime::new(test_config_at(&root))
                .await
                .expect("runtime");
            let result = rt.load(std::slice::from_ref(relative)).await.expect("load");
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.loaded.len(), 1, "{:?}", result.loaded);
            Some((rt, result.loaded[0].module_name.clone()))
        }
        None => None,
    };

    let absolute_rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime");
    let absolute_result = absolute_rt
        .load(std::slice::from_ref(&root))
        .await
        .expect("load");
    assert!(
        absolute_result.errors.is_empty(),
        "{:?}",
        absolute_result.errors
    );
    assert_eq!(
        absolute_result.loaded.len(),
        1,
        "{:?}",
        absolute_result.loaded
    );
    let absolute_name = absolute_result.loaded[0].module_name.clone();
    assert!(
        pir::extensions::loader::module_name_to_path(&absolute_name)
            .is_some_and(|path| Path::new(path).is_absolute()),
        "module id must be absolute: {absolute_name}"
    );

    // The anchoring guarantee (only comparable when both spellings exist):
    // both spellings reach the same module id.
    if let Some((_, relative_name)) = &relative_rt {
        assert_eq!(
            relative_name, &absolute_name,
            "module id must not depend on the spelling"
        );
    }

    // Both imports (relative sibling + bare `node_modules` dep) executed;
    // the absolute half always runs.
    for rt in [Some(&absolute_rt), relative_rt.as_ref().map(|(rt, _)| rt)]
        .into_iter()
        .flatten()
    {
        let value = rt
            .eval_json("JSON.stringify(globalThis.__rel_ext_loaded)")
            .await
            .expect("eval");
        assert_eq!(value, serde_json::json!("rel-ok/dep-ok"), "{value:?}");
    }

    let _ = std::fs::remove_dir_all(root.parent().unwrap());
}

/// A CommonJS extension entry (`package.json` → `index.js` with
/// `module.exports = function (pi) {...}`) must load through the CJS bridge:
/// the runtime used to transpile every entry as ESM, so a CJS entry died on
/// `module is not defined` before its factory could run, and its relative
/// `require("./helper.js")` never resolved.
#[tokio::test(flavor = "current_thread")]
async fn cjs_entry_loads_and_its_factory_runs() {
    let dir = std::env::temp_dir().join(format!("pir-cjs-entry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("pi-cjs-ext");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "pi-cjs-ext", "version": "1.0.0", "pi": { "extensions": ["./index.js"] } }"#,
    )
    .expect("manifest");
    std::fs::write(root.join("helper.js"), "module.exports = { value: 42 };\n").expect("helper");
    std::fs::write(
        root.join("index.js"),
        "const helper = require(\"./helper.js\");\n\
         module.exports = (pi) => {\n\
           globalThis.__cjs_ext_loaded = helper.value;\n\
         };\n",
    )
    .expect("entry");

    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime");
    let result = rt.load(std::slice::from_ref(&root)).await.expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.loaded.len(), 1, "{:?}", result.loaded);
    let value = rt
        .eval_json("JSON.stringify(globalThis.__cjs_ext_loaded)")
        .await
        .expect("eval");
    assert_eq!(
        value,
        serde_json::json!(42),
        "the CJS factory must run and see its require'd sibling"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
