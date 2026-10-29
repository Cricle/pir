//! Unit tests migrated out of `src/extensions/npm_resolve.rs` inline
//! `#[cfg(test)]` module: Node-style bare-specifier resolution (`exports`,
//! `imports`, legacy `main`/index probing, search-root fallback), module-kind
//! detection, and the lexical ESM-syntax scan.
//!
//! Every item asserted below is already `pub` on
//! `pir::extensions::loader::npm_resolve`, so no source-side visibility
//! change was needed for this file.

use std::path::{Path, PathBuf};

use pir::extensions::loader::npm_resolve::{
    has_module_syntax, module_kind, resolve_bare, resolve_bare_searching, resolve_path_specifier,
    ModuleKind, ResolveError, ResolveMode,
};

/// Unique fixture root per test (temp + pid + name).
fn fixture_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-npm-resolve-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture root");
    dir
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
    std::fs::write(path, content).expect("write fixture");
}

fn exports_pkg(root: &Path) -> PathBuf {
    let pkg = root.join("node_modules").join("dual");
    write(
        &root.join("node_modules"),
        "dual/package.json",
        r#"{ "name": "dual", "exports": {
            ".": { "types": "./t.d.ts", "node": { "import": "./dist/index.mjs", "require": "./dist/index.cjs" }, "default": "./dist/fallback.js" },
            "./sub": "./dist/sub.mjs",
            "./features/*": { "node": "./dist/features/*.js" }
        } }"#,
    );
    for file in [
        "dist/index.mjs",
        "dist/index.cjs",
        "dist/fallback.js",
        "dist/sub.mjs",
        "dist/features/a.js",
        "dist/features/deep/b.js",
    ] {
        write(
            &root.join("node_modules"),
            &format!("dual/{file}"),
            "// ok\n",
        );
    }
    pkg
}

#[test]
fn exports_conditions_split_by_mode() {
    let root = fixture_root("conditions");
    let pkg = exports_pkg(&root);
    let esm = resolve_bare(&root, "dual", ResolveMode::Esm).expect("esm root");
    assert_eq!(esm.path, pkg.join("dist/index.mjs"));
    assert_eq!(esm.package_root, pkg);
    assert_eq!(esm.package_name, "dual");
    let cjs = resolve_bare(&root, "dual", ResolveMode::Require).expect("require root");
    assert_eq!(cjs.path, pkg.join("dist/index.cjs"));
    let sub = resolve_bare(&root, "dual/sub", ResolveMode::Esm).expect("subpath");
    assert_eq!(sub.path, pkg.join("dist/sub.mjs"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exports_pattern_uses_longest_prefix() {
    let root = fixture_root("patterns");
    let pkg = exports_pkg(&root);
    let flat = resolve_bare(&root, "dual/features/a", ResolveMode::Esm).expect("flat");
    assert_eq!(flat.path, pkg.join("dist/features/a.js"));
    let deep = resolve_bare(&root, "dual/features/deep/b", ResolveMode::Esm).expect("deep");
    assert_eq!(deep.path, pkg.join("dist/features/deep/b.js"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exports_rejects_invalid_and_escaping_targets() {
    let root = fixture_root("targets");
    write(
        &root.join("node_modules"),
        "bad/package.json",
        r#"{ "name": "bad", "exports": { ".": "not-relative.js" } }"#,
    );
    write(&root.join("node_modules"), "bad/not-relative.js", "// x\n");
    let error = resolve_bare(&root, "bad", ResolveMode::Esm).expect_err("bare target");
    assert_eq!(error.code(), "ERR_INVALID_PACKAGE_TARGET");
    write(
        &root.join("node_modules"),
        "escape/package.json",
        r#"{ "name": "escape", "exports": { ".": "./../outside.js" } }"#,
    );
    write(&root, "outside.js", "// x\n");
    let error = resolve_bare(&root, "escape", ResolveMode::Esm).expect_err("escape");
    assert!(
        error.to_string().contains("ERR_INVALID_PACKAGE_TARGET"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exports_no_match_reports_subpath() {
    let root = fixture_root("nomatch");
    exports_pkg(&root);
    let error = resolve_bare(&root, "dual/missing", ResolveMode::Esm).expect_err("missing");
    assert!(
        matches!(error, ResolveError::NotExported { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("no matching export './missing'"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn legacy_main_index_and_subpaths() {
    let root = fixture_root("legacy");
    write(
        &root.join("node_modules"),
        "legacy/package.json",
        r#"{ "name": "legacy", "main": "./lib/entry.js" }"#,
    );
    write(
        &root.join("node_modules"),
        "legacy/lib/entry.js",
        "// main\n",
    );
    write(
        &root.join("node_modules"),
        "legacy/nested/leaf.js",
        "// leaf\n",
    );
    let main = resolve_bare(&root, "legacy", ResolveMode::Esm).expect("main");
    assert_eq!(main.path, root.join("node_modules/legacy/lib/entry.js"));
    let leaf = resolve_bare(&root, "legacy/nested/leaf", ResolveMode::Esm).expect("subpath");
    assert_eq!(leaf.path, root.join("node_modules/legacy/nested/leaf.js"));
    write(
        &root.join("node_modules"),
        "plain/package.json",
        r#"{ "name": "plain" }"#,
    );
    write(&root.join("node_modules"), "plain/index.js", "// index\n");
    let index = resolve_bare(&root, "plain", ResolveMode::Esm).expect("index");
    assert_eq!(index.path, root.join("node_modules/plain/index.js"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_package_reports_importer() {
    let root = fixture_root("missing");
    let nested = root.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let error = resolve_bare(&nested, "left-pad", ResolveMode::Esm).expect_err("missing");
    assert!(matches!(error, ResolveError::NotFound { .. }));
    let text = error.to_string();
    assert!(text.contains("cannot find package 'left-pad'"), "{text}");
    assert!(text.contains("deep"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scoped_package_and_walk_up_from_inside_node_modules() {
    let root = fixture_root("scoped");
    write(
        &root.join("node_modules"),
        "@scope/tool/package.json",
        r#"{ "name": "@scope/tool", "exports": { ".": "./main.mjs" } }"#,
    );
    write(
        &root.join("node_modules"),
        "@scope/tool/main.mjs",
        "// main\n",
    );
    write(
        &root.join("node_modules"),
        "@scope/tool/inner/package.json",
        r#"{ "name": "inner" }"#,
    );
    // Importer inside node_modules keeps walking up past its own package.
    let importer = root.join("node_modules").join("@scope/tool/inner/deep");
    std::fs::create_dir_all(&importer).unwrap();
    let resolved = resolve_bare(&importer, "@scope/tool", ResolveMode::Esm).expect("scoped");
    assert_eq!(
        resolved.path,
        root.join("node_modules/@scope/tool/main.mjs")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn native_addon_resolution_is_rejected() {
    let root = fixture_root("native");
    write(
        &root.join("node_modules"),
        "native/package.json",
        r#"{ "name": "native", "main": "./build/Release/addon.node" }"#,
    );
    write(
        &root.join("node_modules"),
        "native/build/Release/addon.node",
        "x",
    );
    let error = resolve_bare(&root, "native", ResolveMode::Esm).expect_err("native");
    assert_eq!(error.code(), "ERR_DLOPEN_DISABLED");
    let direct = resolve_path_specifier(&root, "./node_modules/native/build/Release/addon.node");
    assert_eq!(
        direct.expect_err("native direct").code(),
        "ERR_DLOPEN_DISABLED"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn module_kind_follows_package_type_and_extensions() {
    let root = fixture_root("kind");
    write(&root, "esm/package.json", r#"{ "type": "module" }"#);
    write(&root, "esm/mod.js", "// esm\n");
    write(&root, "cjs/package.json", r#"{ "name": "cjs" }"#);
    write(&root, "cjs/mod.js", "// cjs\n");
    write(&root, "bare.js", "// no package.json\n");
    assert_eq!(module_kind(&root.join("esm/mod.js")), ModuleKind::Esm);
    assert_eq!(module_kind(&root.join("cjs/mod.js")), ModuleKind::Cjs);
    assert_eq!(module_kind(&root.join("bare.js")), ModuleKind::Cjs);
    assert_eq!(module_kind(&root.join("x.mjs")), ModuleKind::Esm);
    assert_eq!(module_kind(&root.join("x.cjs")), ModuleKind::Cjs);
    assert_eq!(module_kind(&root.join("x.json")), ModuleKind::Json);
    assert_eq!(module_kind(&root.join("x.node")), ModuleKind::Native);
    assert_eq!(module_kind(&root.join("x.ts")), ModuleKind::Esm);
    assert_eq!(module_kind(&root.join("x.mts")), ModuleKind::Esm);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn path_specifier_probes_extensions_and_directories() {
    let root = fixture_root("paths");
    write(&root, "a/b.js", "// b\n");
    write(&root, "dir/index.js", "// index\n");
    assert_eq!(
        resolve_path_specifier(&root, "./a/b").expect("ext probe"),
        root.join("a").join("b.js")
    );
    assert_eq!(
        resolve_path_specifier(&root, "./dir").expect("dir index"),
        root.join("dir/index.js")
    );
    let error = resolve_path_specifier(&root, "./nope").expect_err("missing");
    assert_eq!(error.code(), "ERR_MODULE_NOT_FOUND");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_as_file_appends_extension_after_named_extension() {
    // Node's LOAD_AS_FILE: `require("./util.inspect")` finds
    // "util.inspect.js" — the extension is appended unconditionally, and
    // a named extension on the base is data, not a stop signal.
    let root = fixture_root("named-ext");
    write(&root, "lib/util.inspect.js", "module.exports = 42;\n");
    write(&root, "lib/data.json", "{}\n");
    assert_eq!(
        resolve_path_specifier(&root.join("lib"), "./util.inspect").expect("dotted probe"),
        root.join("lib").join("util.inspect.js")
    );
    // An exact hit still wins over the appended candidates.
    write(&root, "lib/exact", "data");
    assert_eq!(
        resolve_path_specifier(&root.join("lib"), "./exact").expect("exact"),
        root.join("lib").join("exact")
    );
    let error = resolve_path_specifier(&root.join("lib"), "./absent.inspect").expect_err("missing");
    assert_eq!(error.code(), "ERR_MODULE_NOT_FOUND");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn package_imports_resolves_fragment_specifiers() {
    let root = fixture_root("imports");
    let pkg = root.join("vendor").join("chalk-like");
    write(
        &pkg,
        "package.json",
        r##"{ "name": "chalk-like", "type": "module", "imports": {
            "#ansi-styles": "./source/vendor/ansi-styles/index.js",
            "#supports-color": { "node": "./source/vendor/supports-color/index.js", "default": "./source/vendor/supports-color/browser.js" },
            "#styles/*": "./source/vendor/*.js"
        } }"##,
    );
    write(&pkg, "source/index.js", "import '#ansi-styles';\n");
    write(&pkg, "source/vendor/ansi-styles/index.js", "// ansi\n");
    write(&pkg, "source/vendor/supports-color/index.js", "// node\n");
    write(
        &pkg,
        "source/vendor/supports-color/browser.js",
        "// browser\n",
    );
    write(&pkg, "source/vendor/palette.js", "// palette\n");
    let source = pkg.join("source");
    let ansi = resolve_bare(&source, "#ansi-styles", ResolveMode::Esm).expect("exact");
    assert_eq!(ansi.path, pkg.join("source/vendor/ansi-styles/index.js"));
    assert_eq!(ansi.package_root, pkg);
    assert_eq!(ansi.package_name, "chalk-like");
    // Condition object: the "node" branch wins under ESM conditions.
    let supports = resolve_bare(&source, "#supports-color", ResolveMode::Esm).expect("conditions");
    assert_eq!(
        supports.path,
        pkg.join("source/vendor/supports-color/index.js")
    );
    // Pattern key with the wildcard substitution.
    let pattern = resolve_bare(&source, "#styles/palette", ResolveMode::Esm).expect("pattern");
    assert_eq!(pattern.path, pkg.join("source/vendor/palette.js"));
    // Unmatched fragment: ERR_PACKAGE_IMPORT_NOT_DEFINED.
    let error = resolve_bare(&source, "#absent", ResolveMode::Esm).expect_err("missing");
    assert_eq!(error.code(), "ERR_PACKAGE_IMPORT_NOT_DEFINED");
    // No "imports" field in the nearest manifest: same error.
    let plain = root.join("vendor").join("plain");
    write(&plain, "package.json", r#"{ "name": "plain" }"#);
    write(&plain, "index.js", "// x\n");
    let error = resolve_bare(&plain, "#anything", ResolveMode::Esm).expect_err("no imports");
    assert_eq!(error.code(), "ERR_PACKAGE_IMPORT_NOT_DEFINED");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn package_imports_probes_typescript_source_extensions() {
    // Publish-source packages ship `.ts` and address it through
    // `"#src/*": "./src/*"`; the probe must complete the extension with
    // `ts` (the loader transpiles the hit) instead of only JS extensions,
    // or every `#src/...` specifier reports ERR_MODULE_NOT_FOUND and the
    // extension never loads (@gotgenes/pi-permission-system).
    let root = fixture_root("imports-ts");
    let pkg = root.join("vendor").join("permission-system");
    write(
        &pkg,
        "package.json",
        r##"{ "name": "permission-system", "type": "module", "imports": {
            "#src/*": "./src/*"
        } }"##,
    );
    write(&pkg, "src/foo.ts", "export const foo = 1;\n");
    write(&pkg, "src/nested/index.ts", "export const nested = 1;\n");
    let source = pkg.join("src");
    let hit = resolve_bare(&source, "#src/foo", ResolveMode::Esm).expect("ts probe");
    assert_eq!(hit.path, pkg.join("src/foo.ts"));
    assert_eq!(hit.package_root, pkg);
    assert_eq!(hit.package_name, "permission-system");
    // `.ts` lands in the loader's transpiling ESM path.
    assert_eq!(module_kind(&hit.path), ModuleKind::Esm);
    // Directory target: `index.ts` fallback.
    let index = resolve_bare(&source, "#src/nested", ResolveMode::Esm).expect("index.ts");
    assert_eq!(index.path, pkg.join("src/nested/index.ts"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_roots_back_the_node_modules_walk_up() {
    // The `pir js` fallback: an importer with no node_modules ancestor
    // resolves through the extra search root (project-local wins first).
    let root = fixture_root("search-roots");
    let script_dir = root.join("workspace").join("src");
    std::fs::create_dir_all(&script_dir).unwrap();
    let npm_root = root.join("agent").join("npm");
    write(
        &npm_root.join("node_modules"),
        "remote/index.js",
        "// remote\n",
    );
    write(
        &root.join("workspace").join("node_modules"),
        "local/index.js",
        "// local\n",
    );
    let roots = vec![npm_root.clone()];
    let fallback = resolve_bare_searching(&script_dir, &roots, "remote", ResolveMode::Esm)
        .expect("search-root fallback");
    assert_eq!(fallback.path, npm_root.join("node_modules/remote/index.js"));
    assert_eq!(fallback.package_root, npm_root.join("node_modules/remote"));
    // Walk-up from the importer still wins over the search root.
    let local =
        resolve_bare_searching(&script_dir, &roots, "local", ResolveMode::Esm).expect("walk-up");
    assert_eq!(
        local.path,
        root.join("workspace/node_modules/local/index.js")
    );
    // Neither present: NotFound with the original importer named.
    let error = resolve_bare_searching(&script_dir, &roots, "absent", ResolveMode::Esm)
        .expect_err("missing");
    assert!(matches!(error, ResolveError::NotFound { .. }), "{error:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn module_syntax_detection_for_ambiguous_js() {
    let root = fixture_root("detect");
    // No package.json anywhere: source with import/export is ESM.
    write(
        &root,
        "esm-like.js",
        "import { x } from './y.js';\nconsole.log(x);\n",
    );
    assert_eq!(module_kind(&root.join("esm-like.js")), ModuleKind::Esm);
    write(&root, "reexport.js", "export { x } from './y.js';\n");
    assert_eq!(module_kind(&root.join("reexport.js")), ModuleKind::Esm);
    write(&root, "meta.js", "console.log(import.meta.url);\n");
    assert_eq!(module_kind(&root.join("meta.js")), ModuleKind::Esm);
    // Plain CommonJS keeps CJS. Strings/comments/members don't trip it,
    // and dynamic import is valid CJS.
    write(
        &root,
        "cjs-like.js",
        "const s = 'import not real';\n// export in a comment\nconst p = require('path');\nfunction load(name) { return import(name); }\nmodule.exports = { p, load };\n",
    );
    assert_eq!(module_kind(&root.join("cjs-like.js")), ModuleKind::Cjs);
    // Explicit "type" fields decide regardless of source contents.
    write(&root, "typed-esm/package.json", r#"{ "type": "module" }"#);
    write(&root, "typed-esm/mod.js", "module.exports = 1;\n");
    assert_eq!(module_kind(&root.join("typed-esm/mod.js")), ModuleKind::Esm);
    write(&root, "typed-cjs/package.json", r#"{ "type": "commonjs" }"#);
    write(&root, "typed-cjs/mod.js", "import { x } from './y.js';\n");
    assert_eq!(module_kind(&root.join("typed-cjs/mod.js")), ModuleKind::Cjs);
    // Manifest without a "type" field: detection applies under it too.
    write(&root, "no-type/package.json", r#"{ "name": "no-type" }"#);
    write(&root, "no-type/mod.js", "export const x = 1;\n");
    assert_eq!(module_kind(&root.join("no-type/mod.js")), ModuleKind::Esm);
    write(&root, "no-type/plain.js", "const x = 1;\n");
    assert_eq!(module_kind(&root.join("no-type/plain.js")), ModuleKind::Cjs);
    let _ = std::fs::remove_dir_all(&root);
}

/// G12: the lexical detect-module scan must not mistake non-ESM uses of
/// the `import`/`export` words for module syntax (Node decides through a
/// real parse; these are the false-positive classes the lexical scan has
/// to cover: regex literals, object keys, string content, comments).
#[test]
fn module_syntax_scan_rejects_non_esm_keyword_uses() {
    // Regex literal bodies containing the keywords.
    assert!(!has_module_syntax(
        "if (/export\\s+default/.test(src)) rewrite();\n"
    ));
    assert!(!has_module_syntax(
        "const m = src.match(/import .* from/);\n"
    ));
    assert!(!has_module_syntax("const re = /a[import/export]b/g;\n"));
    // Division stays division; a following real regex still gets skipped.
    assert!(!has_module_syntax(
        "const half = total / 2; const re = /export/;\n"
    ));
    // Regex in expression position (after `(`).
    assert!(!has_module_syntax(
        "const flags = 'i'; const re = return_path(/export/);\n"
    ));
    // Object keys spelled with the keywords.
    assert!(!has_module_syntax("const t = { import: 1, export: 2 };\n"));
    // `import x = require(...)` is TS-only (erased before this scan).
    assert!(!has_module_syntax("const mod = { load };\n"));
    // Real module syntax still detected.
    assert!(has_module_syntax("export const x = 1;\n"));
    assert!(has_module_syntax("import { x } from './y.js';\n"));
    assert!(has_module_syntax(
        "const u = new URL('.', import.meta.url);\n"
    ));
    assert!(has_module_syntax("export default function () {}\n"));
    // A regex false-positive must not mask a real export elsewhere.
    assert!(has_module_syntax(
        "const re = /export/;\nexport const x = 1;\n"
    ));
}
