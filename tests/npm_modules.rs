//! Phase 4a integration tests: npm dependency resolution + the CommonJS
//! runtime (`createRequire`, `require`, the loader's CJS/JSON bridges).
//!
//! Fixture packages are generated at runtime under `%TEMP%` (the repository
//! hygiene gate forbids tracked `.js`/`.json` files, so on-disk fixtures
//! cannot be committed):
//!
//! ```text
//! <root>/node_modules/esm-pkg      (type: module, exports "." + "./sub")
//! <root>/node_modules/cjs-pkg      (type: commonjs, module.exports object)
//! <root>/node_modules/cycle-a      (a.js <-> b.js cycle, b.js requires cjs-pkg)
//! <root>/node_modules/dual-pkg     (exports {import, require} split)
//! <root>/node_modules/native-pkg   (main points at a .node addon)
//! <root>/ext/main.ts               (the fixture extension; imports + require)
//! <root>/ext/data.json, only-esm.mjs
//! <root>/bad-ext/index.ts          (bare package that does not exist)
//! <root>/scan-ext/index.ts         (node:cluster — outside the shimmed table)
//! ```

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// Unique fixture root per test (temp + pid + name), fully regenerated.
fn fixture_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-npm-modules-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture root");
    dir
}

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture subdir");
    }
    std::fs::write(path, content).expect("write fixture file");
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

/// The npm tree every fixture test loads against.
fn write_npm_tree(root: &Path) {
    // 1) ESM package with an exports map (condition object + subpath).
    write_file(
        root,
        "node_modules/esm-pkg/package.json",
        r#"{ "name": "esm-pkg", "type": "module", "exports": {
            ".": { "node": "./dist/index.mjs" },
            "./sub": "./dist/sub.mjs"
        } }"#,
    );
    write_file(
        root,
        "node_modules/esm-pkg/dist/index.mjs",
        "export default { name: \"esm-default\" };\nexport const value = \"esm-named\";\n",
    );
    write_file(
        root,
        "node_modules/esm-pkg/dist/sub.mjs",
        "export default \"esm-sub\";\n",
    );
    // 2) CommonJS package (default-only interop from ESM + require).
    write_file(
        root,
        "node_modules/cjs-pkg/package.json",
        r#"{ "name": "cjs-pkg", "type": "commonjs", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/cjs-pkg/index.js",
        "function greet(name) { return \"hello \" + name; }\n\
         module.exports = { greet: greet };\n\
         module.exports.extra = 42;\n",
    );
    // 3) require cycle: a -> b -> a (partial exports) and b -> cjs-pkg.
    write_file(
        root,
        "node_modules/cycle-a/package.json",
        r#"{ "name": "cycle-a", "main": "./a.js" }"#,
    );
    write_file(
        root,
        "node_modules/cycle-a/a.js",
        "exports.name = \"a\";\n\
         const b = require(\"./b\");\n\
         exports.partialFromB = b.partial;\n\
         exports.cjsGreet = b.cjsGreet;\n\
         exports.done = \"a->\" + b.done;\n",
    );
    write_file(
        root,
        "node_modules/cycle-a/b.js",
        "const a = require(\"./a\");\n\
         const cjs = require(\"cjs-pkg\");\n\
         exports.partial = a.name;\n\
         exports.done = \"b(\" + String(a.done) + \")\";\n\
         exports.cjsGreet = cjs.greet(\"b\");\n",
    );
    // 4) Conditional split: ESM import picks index.mjs, require picks index.cjs.
    write_file(
        root,
        "node_modules/dual-pkg/package.json",
        r#"{ "name": "dual-pkg", "exports": {
            ".": { "import": "./index.mjs", "require": "./index.cjs" }
        } }"#,
    );
    write_file(
        root,
        "node_modules/dual-pkg/index.mjs",
        "export default { flavor: \"esm\" };\n",
    );
    write_file(
        root,
        "node_modules/dual-pkg/index.cjs",
        "module.exports = { flavor: \"cjs\" };\n",
    );
    // 8) A package whose main lands on a native addon.
    write_file(
        root,
        "node_modules/native-pkg/package.json",
        r#"{ "name": "native-pkg", "main": "./addon.node" }"#,
    );
    write_file(
        root,
        "node_modules/native-pkg/addon.node",
        "fake native addon placeholder\n",
    );
}

/// The extension under test: ESM entry importing packages, requiring CJS
/// through `createRequire(import.meta.url)`, reporting everything through a
/// tool and a global.
const NPM_EXT_TS: &str = r#"
import { createRequire } from "node:module";
import { URL as URLImpl } from "node:url";
import esmDefault, { value } from "esm-pkg";
import esmSub from "esm-pkg/sub";
import cjsFromEsm from "cjs-pkg";
import dual from "dual-pkg";
import data from "./data.json";

const require = createRequire(import.meta.url);

export default (pi: any) => {
  const out: any = {};
  out.esmDefaultName = esmDefault.name;
  out.esmNamed = value;
  out.esmSub = esmSub;
  out.cjsFromEsmGreet = cjsFromEsm.greet("esm");
  out.cjsFromEsmExtra = cjsFromEsm.extra;
  out.dualEsm = dual.flavor;
  out.dataAnswer = data.answer;

  const cjs = require("cjs-pkg");
  out.cjsGreet = cjs.greet("w");
  out.cjsExtra = cjs.extra;
  out.resolvePath = require.resolve("cjs-pkg").split("\\").join("/");
  out.dualCjs = require("dual-pkg").flavor;
  out.dualCjsSecond = require("dual-pkg").flavor;
  out.dataRequire = require("./data.json").answer;

  const cycle = require("cycle-a");
  out.cyclePartial = cycle.partialFromB;
  out.cycleDone = cycle.done;
  out.cycleGreet = cycle.cjsGreet;

  out.pathJoin = require("path").join("a", "b");
  out.pathJoinNode = require("node:path").join("a", "b");
  out.fsType = typeof require("fs").readFileSync;
  out.moduleCreateRequire = typeof require("module").createRequire;

  // Deep directory inside a package: createRequire from a subpath must still
  // walk up to the shared node_modules.
  const deep = createRequire(
    new URLImpl("../node_modules/cjs-pkg/lib/inner/probe.js", import.meta.url),
  );
  out.deepResolve = deep.resolve("cjs-pkg").split("\\").join("/");
  out.deepExtra = deep("cjs-pkg").extra;

  try {
    // require(esm), Node 22.12+: the ES module evaluates synchronously and
    // the namespace comes back on the CJS side (no default export here, so
    // no default-unwrap applies).
    const esm = require("./only-esm.mjs");
    out.requireEsm = "loaded:" + esm.nope + "/" + esm.hidden + "/" + esm.renamed;
  } catch (error: any) {
    out.requireEsm = error.code;
  }

  try {
    require("native-pkg");
    out.nativeCode = "no-throw";
  } catch (error: any) {
    out.nativeCode = error.code;
    out.nativeMessage = error.message;
  }

  let readSourceCode = null;
  try {
    (globalThis as any).__pirNodeCall("npm", "readSource", {
      path: (import.meta as any).dirname + "/../node_modules/native-pkg/addon.node",
    });
  } catch (error: any) {
    readSourceCode = error.code;
  }
  out.readSourceCode = readSourceCode;

  (globalThis as any).__npmResults = out;

  pi.registerTool({
    name: "npm-echo",
    description: "Return npm resolution results",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: JSON.stringify(out) }] }),
  });
};
"#;

fn write_main_fixture(root: &Path) -> PathBuf {
    write_npm_tree(root);
    write_file(root, "ext/data.json", "{ \"answer\": 42 }\n");
    // Named export CLAUSES + a named-import shorthand: the require(esm)
    // transform once emitted `exports[parse] = parse` (bracket access
    // with a bare identifier) and `{ "x" } = ns` (invalid quoted
    // destructuring shorthand) — both shipped broken namespaces.
    write_file(root, "ext/esm-dep.mjs", "export const hidden = 2;\n");
    write_file(
        root,
        "ext/only-esm.mjs",
        "import { hidden } from \"./esm-dep.mjs\";\nexport const nope = 1;\nexport { hidden, nope as renamed };\n",
    );
    write_file(root, "ext/main.ts", NPM_EXT_TS);
    root.join("ext/main.ts")
}

/// 9a) Scan-level: bare npm specifiers defer to load time; `node:` builtins
/// outside the shimmed table stay rejected at scan time. (The unenv wave put
/// every real v24 builtin in the table — throws tier included — so the
/// rejection path is now exercised with a non-builtin `node:` specifier.)
#[test]
fn scan_allows_bare_packages_and_rejects_unknown_node_builtins() {
    let deps = pir::extensions::scan_dependencies(
        r#"
        import "yaml";
        const x = await import("@scope/pkg/sub");
        const fs = require("fs");
        import "node:cluster";
        import "node:nonexistent_builtin";
        "#,
    );
    let find = |specifier: &str| {
        deps.iter()
            .find(|dep| dep.specifier == specifier)
            .unwrap_or_else(|| panic!("missing dependency {specifier}: {deps:?}"))
    };
    assert!(find("yaml").is_supported());
    assert!(find("@scope/pkg/sub").is_supported());
    assert!(find("fs").is_supported());
    assert!(
        find("node:cluster").is_supported(),
        "throws-tier builtins load past the scan"
    );
    assert!(
        !find("node:nonexistent_builtin").is_supported(),
        "node: builtins outside the table stay scan-rejected"
    );
}

/// 1–8, 10) One loaded extension exercising ESM/CJS/JSON imports, subpaths,
/// conditional exports, `createRequire`, cycles, builtins and the native
/// rejection — asserted through both `globalThis` and a registered tool.
#[tokio::test(flavor = "current_thread")]
async fn npm_fixture_loads_esm_cjs_json_cycles_and_create_require() {
    let root = fixture_root("main");
    let entry = write_main_fixture(&root);
    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime construction");

    let result = rt.load(std::slice::from_ref(&entry)).await.expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.loaded.len(), 1);

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__npmResults ?? null)")
        .await
        .expect("read __npmResults");
    assert_eq!(v["esmDefaultName"], "esm-default", "{v}");
    assert_eq!(v["esmNamed"], "esm-named", "{v}");
    assert_eq!(v["esmSub"], "esm-sub", "{v}");
    assert_eq!(v["cjsFromEsmGreet"], "hello esm", "{v}");
    assert_eq!(v["cjsFromEsmExtra"], 42, "{v}");
    assert_eq!(
        v["dualEsm"], "esm",
        "ESM import must pick the import condition: {v}"
    );
    assert_eq!(
        v["dualCjs"], "cjs",
        "require must pick the require condition: {v}"
    );
    assert_eq!(
        v["dualCjsSecond"], "cjs",
        "repeated require must stay cached: {v}"
    );
    assert_eq!(v["dataAnswer"], 42, "ESM JSON default import: {v}");
    assert_eq!(v["dataRequire"], 42, "require('./data.json'): {v}");
    assert_eq!(v["cjsGreet"], "hello w", "{v}");
    assert_eq!(v["cjsExtra"], 42, "{v}");
    let resolve_path = v["resolvePath"].as_str().unwrap_or_default();
    assert!(
        resolve_path.ends_with("node_modules/cjs-pkg/index.js"),
        "require.resolve must return the package file: {v}"
    );
    assert_eq!(
        v["cyclePartial"], "a",
        "cycle peer must see partial exports: {v}"
    );
    assert_eq!(v["cycleDone"], "a->b(undefined)", "{v}");
    assert_eq!(v["cycleGreet"], "hello b", "{v}");
    let sep = if cfg!(windows) { "\\" } else { "/" };
    assert_eq!(
        v["pathJoin"],
        format!("a{sep}b"),
        "require('path') builtin: {v}"
    );
    assert_eq!(
        v["pathJoinNode"],
        format!("a{sep}b"),
        "require('node:path'): {v}"
    );
    assert_eq!(v["fsType"], "function", "require('fs') builtin: {v}");
    assert_eq!(
        v["moduleCreateRequire"], "function",
        "require('module'): {v}"
    );
    let deep_resolve = v["deepResolve"].as_str().unwrap_or_default();
    assert!(
        deep_resolve.ends_with("node_modules/cjs-pkg/index.js"),
        "deep createRequire must walk up: {v}"
    );
    assert_eq!(
        v["deepExtra"], 42,
        "deep createRequire must load the module: {v}"
    );
    assert_eq!(
        v["requireEsm"], "loaded:1/2/1",
        "require(esm) must evaluate the module and return its namespace: {v}"
    );
    assert_eq!(v["nativeCode"], "ERR_DLOPEN_DISABLED", "{v}");
    assert!(
        v["nativeMessage"]
            .as_str()
            .unwrap_or_default()
            .contains("native addons (.node) are not supported in this runtime"),
        "{v}"
    );
    assert_eq!(
        v["readSourceCode"], "ERR_DLOPEN_DISABLED",
        "readSource must refuse native addons: {v}"
    );

    // 10) The registered tool echoes the same results through the tool path.
    let tool_result = rt
        .call_tool("npm-echo", "call-1", &json!({}))
        .await
        .expect("tool executes");
    assert_eq!(tool_result.content.len(), 1);
    let echoed: Value =
        serde_json::from_str(&tool_result.content[0].text).expect("tool JSON payload");
    assert_eq!(echoed["cjsGreet"], "hello w");
    assert_eq!(echoed["esmNamed"], "esm-named");
    let _ = std::fs::remove_dir_all(&root);
}

/// 9b) A bare package that exists nowhere fails at load time (not scan time)
/// with the package-resolution wording.
#[tokio::test(flavor = "current_thread")]
async fn missing_bare_package_fails_at_load_with_package_error() {
    let root = fixture_root("missing");
    write_file(
        &root,
        "bad-ext/index.ts",
        "import \"no-such-pkg-xyz\";\nexport default (pi: any) => {};\n",
    );
    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime construction");
    let result = rt
        .load(&[root.join("bad-ext")])
        .await
        .expect("load returns");
    assert_eq!(result.errors.len(), 1, "{result:?}");
    let error = &result.errors[0].error;
    assert!(
        error.contains("cannot find package 'no-such-pkg-xyz'"),
        "missing package must report the resolution error: {error}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 9c) `node:` builtins outside the shimmed table are still rejected during
/// the entry scan (message unchanged).
#[tokio::test(flavor = "current_thread")]
async fn unknown_node_builtin_still_rejected_at_scan() {
    let root = fixture_root("scan");
    write_file(
        &root,
        "scan-ext/index.ts",
        "import \"node:nonexistent_builtin\";\nexport default (pi: any) => {};\n",
    );
    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime construction");
    let result = rt
        .load(&[root.join("scan-ext")])
        .await
        .expect("load returns");
    assert_eq!(result.errors.len(), 1, "{result:?}");
    let error = &result.errors[0].error;
    assert!(
        error.contains("unsupported dependency `node:nonexistent_builtin`"),
        "scan rejection wording must stay: {error}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Report one installed-extension matrix and enforce the opt-in strictness:
/// with `PIR_REQUIRE_INSTALLED_EXTENSIONS=1` a matrix that found nothing fails
/// instead of reporting green with nothing exercised. Absence stays a skip by
/// default (CI has no `~/.pi` tree).
fn finish_installed_matrix(label: &str, executed: usize, skipped: usize) {
    eprintln!("[npm_modules] {label}: executed={executed} skipped={skipped}");
    if executed == 0 {
        assert!(
            std::env::var_os("PIR_REQUIRE_INSTALLED_EXTENSIONS").is_none(),
            "PIR_REQUIRE_INSTALLED_EXTENSIONS is set but no installed extension fixture was \
             found ({label})"
        );
    }
}

/// Real installed extension packages: the first two are load regressions
/// (hard assert when installed); the rest record their error strings only —
/// phase 4a intentionally moves them past the old scan rejection without
/// promising a full load (native addons / named CJS exports are later work).
#[tokio::test(flavor = "current_thread")]
async fn real_installed_extensions_load_matrix() {
    let cases: &[(&str, &str, bool)] = &[
        ("pi-tool-repair", "tool-repair.ts", true),
        ("@vndv/pi-codegraph", "extensions/codegraph.ts", true),
        ("pi-subagents", "index.js", false),
        ("context-mode", "build/adapters/pi/extension.js", false),
    ];
    let Some(home) = std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .filter(|value| !value.is_empty())
    else {
        eprintln!("[npm_modules] no home directory; skipping real-extension matrix");
        // The four npm packages plus pi-phone.
        finish_installed_matrix("real-extension matrix", 0, cases.len() + 1);
        return;
    };
    let home = PathBuf::from(home);
    let npm_root = home.join(".pi/agent/npm/node_modules");
    let mut regressions: Vec<(String, String)> = Vec::new();
    let mut recorded: Vec<String> = Vec::new();
    let mut executed = 0usize;
    let mut skipped = 0usize;
    for (name, entry, must_load) in cases {
        let path = npm_root.join(name).join(entry);
        if !path.is_file() {
            recorded.push(format!("{name}: skipped (not installed)"));
            skipped += 1;
            continue;
        }
        executed += 1;
        let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let rt = ExtensionRuntime::new(test_config_at(&cwd))
            .await
            .expect("runtime construction");
        let outcome = rt.load(std::slice::from_ref(&path)).await.expect("load");
        let failed = outcome
            .errors
            .iter()
            .map(|error| error.error.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        if outcome.loaded.len() == 1 && outcome.errors.is_empty() {
            recorded.push(format!("{name}: loaded"));
        } else {
            recorded.push(format!("{name}: {failed}"));
        }
        if *must_load {
            regressions.push(((*name).to_string(), failed));
        }
    }
    // pi-phone lives as a local checkout, not an npm package.
    let pi_phone = home.join("Desktop/github/pi-phone/index.ts");
    if pi_phone.is_file() {
        executed += 1;
        let rt = ExtensionRuntime::new(test_config_at(pi_phone.parent().unwrap()))
            .await
            .expect("runtime construction");
        let outcome = rt
            .load(std::slice::from_ref(&pi_phone))
            .await
            .expect("load");
        let failed = outcome
            .errors
            .iter()
            .map(|error| error.error.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        recorded.push(if outcome.loaded.len() == 1 && outcome.errors.is_empty() {
            "pi-phone: loaded".to_string()
        } else {
            format!("pi-phone: {failed}")
        });
    } else {
        skipped += 1;
        recorded.push("pi-phone: skipped (not present)".to_string());
    }

    for line in &recorded {
        eprintln!("[npm_modules] real-extension matrix: {line}");
    }
    finish_installed_matrix("real-extension matrix", executed, skipped);
    for (name, failed) in regressions {
        assert!(failed.is_empty(), "{name} must keep loading: {failed}");
    }
}

// ---------------------------------------------------------------------------
// Phase 4b: CJS named-export bridge (cjs_lexer + the loader's interop wrapper)
// ---------------------------------------------------------------------------

use std::collections::BTreeSet;

use pir::extensions::loader::cjs_lexer::{named_exports, named_exports_in_source};

fn name_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// Pure lexer patterns: member assignments (dot + bracket + nested chain
/// tails), `Object.defineProperty` on both exports spellings, and the name
/// filter (identifiers only, no `default`, no dynamic keys).
#[test]
fn cjs_lexer_collects_static_named_export_patterns() {
    let names = named_exports_in_source(
        r#"
        exports.parse = function () {};
        exports["bracket"] = 1;
        module.exports.extra = 2;
        module.exports.deep.tail = 3;
        Object.defineProperty(exports, "prop", { get: function () { return 4; } });
        Object.defineProperty(module.exports, "second", { value: 5 });
        exports["not-valid-name"] = 6;
        exports.default = 7;
        exports.class = 7;
        exports.await = 7;
        const dynamic = "dyn";
        exports[dynamic] = 8;
        exports.bare;
        function nested() { exports.insideFunction = 9; }
        "#,
    );
    assert_eq!(
        names,
        name_set(&["bracket", "extra", "parse", "prop", "second", "tail"]),
        "only statically detectable identifier-named exports are collected"
    );
}

/// Object-literal keys (shorthand, key/value, string, method, accessor, spread
/// skipped) and the chained `exports = module.exports = {...}` shape.
#[test]
fn cjs_lexer_collects_object_literal_and_chained_forms() {
    let names = named_exports_in_source(
        r#"
        const rest = {};
        module.exports = { alpha: 1, beta, "gamma": 2, ...rest, delta() {}, get epsilon() { return 5; }, default: 6 };
        "#,
    );
    assert_eq!(
        names,
        name_set(&["alpha", "beta", "delta", "epsilon", "gamma"]),
        "literal keys minus spread/dynamic/default"
    );

    let chained = named_exports_in_source("exports = module.exports = { one: 1 };");
    assert_eq!(
        chained,
        name_set(&["one"]),
        "chain keeps the inner object keys"
    );
}

/// File-level walk: relative re-export chains resolve through the npm
/// resolver (extension probing), cycles terminate, bare specifiers stop and
/// unparseable sources degrade to the empty set.
#[test]
fn cjs_lexer_follows_relative_reexports_with_cycles_and_degrades() {
    let root = fixture_root("cjs-lexer-reexport");
    write_file(
        &root,
        "barrel.js",
        "module.exports = require(\"./impl.js\");\n",
    );
    write_file(&root, "impl.js", "exports.foo = 1;\nexports.bar = 2;\n");
    let names = named_exports(&root.join("barrel.js"));
    assert_eq!(names, name_set(&["bar", "foo"]), "{names:?}");

    // Extensionless re-export target: the resolver's candidate order applies.
    write_file(&root, "noext.js", "module.exports = require(\"./impl\");\n");
    assert_eq!(
        named_exports(&root.join("noext.js")),
        name_set(&["bar", "foo"])
    );

    // a -> b -> a: both sides contribute, the walk terminates on the second
    // visit of cycle-a.js (own names + re-exported names are a union).
    write_file(
        &root,
        "cycle-a.js",
        "exports.a = 1;\nmodule.exports = require(\"./cycle-b.js\");\n",
    );
    write_file(
        &root,
        "cycle-b.js",
        "exports.b = 2;\nmodule.exports = require(\"./cycle-a.js\");\n",
    );
    assert_eq!(
        named_exports(&root.join("cycle-a.js")),
        name_set(&["a", "b"])
    );

    // Bare re-exports are not expanded (no node_modules in the fixture).
    write_file(
        &root,
        "bare-target.js",
        "module.exports = require(\"left-pad\");\n",
    );
    assert!(named_exports(&root.join("bare-target.js")).is_empty());

    // A source swc cannot parse yields no names (default-only degradation).
    write_file(&root, "broken.js", "module.exports = = {");
    assert!(named_exports(&root.join("broken.js")).is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

/// CJS packages under the fixture tree: `exports.*` assignments,
/// `Object.defineProperty`, a barrel re-export chain and a top-level-return
/// script (swc rejects the source, the runtime still executes it).
fn write_cjs_named_tree(root: &Path) {
    write_file(
        root,
        "node_modules/cjs-named-pkg/package.json",
        r#"{ "name": "cjs-named-pkg", "type": "commonjs", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/cjs-named-pkg/index.js",
        "function parse(text) { return \"parsed:\" + text; }\n\
         exports.parse = parse;\n\
         module.exports.extra = 42;\n\
         Object.defineProperty(exports, \"prop\", { value: \"prop-value\", enumerable: true });\n\
         exports[\"bracket\"] = \"bracket-value\";\n",
    );
    write_file(
        root,
        "node_modules/barrel-pkg/package.json",
        r#"{ "name": "barrel-pkg", "main": "./barrel.js" }"#,
    );
    write_file(
        root,
        "node_modules/barrel-pkg/barrel.js",
        "module.exports = require(\"./impl.js\");\n",
    );
    write_file(
        root,
        "node_modules/barrel-pkg/impl.js",
        "exports.foo = \"foo-value\";\nexports.bar = function () { return \"bar-value\"; };\n",
    );
    write_file(
        root,
        "node_modules/legacy-script-pkg/package.json",
        r#"{ "name": "legacy-script-pkg", "main": "./legacy.js" }"#,
    );
    // Top-level `return` is legal for the CJS runtime but not parseable by
    // swc: the lexer fails, the loader keeps the default-only wrapper.
    write_file(
        root,
        "node_modules/legacy-script-pkg/legacy.js",
        "if (!globalThis.__legacyRan) { globalThis.__legacyRan = \"yes\"; return; }\n\
         exports.unreached = true;\n",
    );
}

const CJS_NAMED_EXT_TS: &str = r#"
import cjsDefault, { parse, extra, prop, bracket } from "cjs-named-pkg";
import { foo, bar } from "barrel-pkg";
import legacyDefault from "legacy-script-pkg";

export default (pi: any) => {
  const out: any = {};
  out.parse = (parse as any)("x");
  out.extra = extra;
  out.prop = prop;
  out.bracket = bracket;
  out.defaultKind = typeof cjsDefault;
  out.defaultParseIsNamed = (cjsDefault as any).parse === parse;
  out.foo = foo;
  out.bar = (bar as any)();
  out.legacyRan = (globalThis as any).__legacyRan;
  out.legacyKind = typeof legacyDefault;
  (globalThis as any).__cjsNamed = out;
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn cjs_named_exports_link_through_the_interop_wrapper() {
    let root = fixture_root("cjs-named");
    write_cjs_named_tree(&root);
    write_file(&root, "ext/main.ts", CJS_NAMED_EXT_TS);
    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime construction");

    let result = rt.load(&[root.join("ext/main.ts")]).await.expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__cjsNamed ?? null)")
        .await
        .expect("read __cjsNamed");
    assert_eq!(v["parse"], "parsed:x", "{v}");
    assert_eq!(v["extra"], 42, "module.exports.extra: {v}");
    assert_eq!(v["prop"], "prop-value", "Object.defineProperty: {v}");
    assert_eq!(v["bracket"], "bracket-value", "exports[\"bracket\"]: {v}");
    assert_eq!(v["defaultKind"], "object", "default is module.exports: {v}");
    assert_eq!(
        v["defaultParseIsNamed"], true,
        "default.parse and the named binding are the same value: {v}"
    );
    assert_eq!(v["foo"], "foo-value", "barrel re-export: {v}");
    assert_eq!(v["bar"], "bar-value", "barrel re-export function: {v}");
    assert_eq!(
        v["legacyRan"], "yes",
        "the top-level-return script still executed: {v}"
    );
    assert_eq!(
        v["legacyKind"], "object",
        "unparseable-file slice degrades to default-only: {v}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Recursive copy for the real `yaml` package fixture (no symlinks in play).
fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create destination dir");
    for entry in std::fs::read_dir(source)
        .expect("read source dir")
        .flatten()
    {
        let path = entry.path();
        let target = destination.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else if path.is_file() {
            std::fs::copy(&path, &target).expect("copy fixture file");
        }
    }
}

const YAML_NAMED_EXT_TS: &str = r#"
import { parse, stringify } from "yaml";

export default (pi: any) => {
  const out: any = { parseType: typeof parse, stringifyType: typeof stringify };
  const doc: any = parse("answer: 42\n");
  out.parsedAnswer = doc && doc.answer;
  out.stringified = stringify({ answer: 42 }).trim();
  (globalThis as any).__yamlNamed = out;
};
"#;

/// The motivating real barrel: yaml's `dist/index.js` is a wall of
/// `exports.X = ...` assignments. The user's npm tree is copied into the
/// fixture at run time; without it the test skips (no absolute path is baked
/// into the repository).
#[tokio::test(flavor = "current_thread")]
async fn real_yaml_barrel_links_named_exports() {
    let Some(home) = std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .filter(|value| !value.is_empty())
    else {
        eprintln!("[npm_modules] no home directory; skipping real yaml barrel test");
        return;
    };
    let yaml_source = PathBuf::from(home).join(".pi/agent/npm/node_modules/yaml");
    if !yaml_source.join("dist/index.js").is_file() {
        eprintln!("[npm_modules] yaml not installed under ~/.pi/agent/npm; skipping");
        return;
    }
    let root = fixture_root("real-yaml");
    copy_tree(&yaml_source, &root.join("node_modules/yaml"));
    write_file(&root, "ext/main.ts", YAML_NAMED_EXT_TS);
    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime construction");

    let result = rt.load(&[root.join("ext/main.ts")]).await.expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__yamlNamed ?? null)")
        .await
        .expect("read __yamlNamed");
    assert_eq!(v["parseType"], "function", "named `parse` must link: {v}");
    assert_eq!(
        v["stringifyType"], "function",
        "named `stringify` must link: {v}"
    );
    assert_eq!(v["parsedAnswer"], 42, "{v}");
    assert_eq!(v["stringified"], "answer: 42", "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Phase 4c: TypeScript/webpack deferred re-exports (`__exportStar`,
// `__createBinding`) and UMD factory surfaces.
//
// The motivating package is `vscode-languageserver-protocol@3.18.4`: its CJS
// entry (`lib/common/api.js`, a TS compile product) puts `DiagnosticSeverity`
// et al. on `exports` only through `__exportStar(require("vscode-languageserver-
// types"), exports)`, whose target is the UMD build of another package — a
// static scan that stops at the helper call reports none of those names, and
// the ESM side then fails to link (`Could not find export 'DiagnosticSeverity'`).
// ---------------------------------------------------------------------------

/// The TS compiler's inline helper prelude, verbatim in shape: the helper
/// *definitions* must not be mistaken for exports (their `__createBinding`
/// passes a dynamic loop variable and they live in `var` statements the walk
/// does not visit), and the `__esModule` marker is not a binding either.
const TS_HELPER_PRELUDE: &str = r#""use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) { if (k2 === undefined) k2 = k; o[k2] = m[k]; }));
var __exportStar = (this && this.__exportStar) || function(m, exports) {
    for (var p in m) if (p !== "default" && !Object.prototype.hasOwnProperty.call(exports, p)) __createBinding(exports, m, p);
};
Object.defineProperty(exports, "__esModule", { value: true });
"#;

/// Fixture packages for the deferred-re-export forms.
///
/// ```text
/// <root>/node_modules/reexport-source-pkg/index.js  exports.alpha / exports.beta
/// <root>/node_modules/reexport-star-pkg/index.js    __exportStar(require("reexport-source-pkg"), exports)
/// <root>/node_modules/reexport-chain-pkg/index.js   var mid = require("./mid"); __exportStar(mid, exports)
/// <root>/node_modules/reexport-chain-pkg/mid.js     __exportStar(require("./leaf"), exports)
/// <root>/node_modules/reexport-chain-pkg/leaf.js    exports.leafName
/// <root>/node_modules/reexport-cycle-pkg/entry.js  __exportStar(require("./a"), exports)
/// <root>/node_modules/reexport-cycle-pkg/a.js       __exportStar(require("./b")) + own name
/// <root>/node_modules/reexport-cycle-pkg/b.js       __exportStar(require("./a")) + own name
/// <root>/node_modules/reexport-bind-pkg/index.js    __createBinding(exports, m, "alpha", "alphaAlias")
/// <root>/node_modules/reexport-missing-pkg/index.js __exportStar(require("no-such-pkg-xyz"), exports)
/// <root>/node_modules/reexport-umd-pkg/index.js     UMD factory `function (require, exports)`
/// ```
fn write_reexport_tree(root: &Path) {
    write_file(
        root,
        "node_modules/reexport-source-pkg/package.json",
        r#"{ "name": "reexport-source-pkg", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-source-pkg/index.js",
        "exports.alpha = \"alpha-value\";\nexports.beta = \"beta-value\";\n",
    );
    // a) Single-level `__exportStar(require("<bare>"), exports)`.
    write_file(
        root,
        "node_modules/reexport-star-pkg/package.json",
        r#"{ "name": "reexport-star-pkg", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-star-pkg/index.js",
        &format!("{TS_HELPER_PRELUDE}__exportStar(require(\"reexport-source-pkg\"), exports);\n"),
    );
    // b) Two levels of chaining, the outer one ident-bound.
    write_file(
        root,
        "node_modules/reexport-chain-pkg/package.json",
        r#"{ "name": "reexport-chain-pkg", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-chain-pkg/index.js",
        &format!("{TS_HELPER_PRELUDE}var mid = require(\"./mid\");\n__exportStar(mid, exports);\n"),
    );
    write_file(
        root,
        "node_modules/reexport-chain-pkg/mid.js",
        &format!("{TS_HELPER_PRELUDE}__exportStar(require(\"./leaf\"), exports);\n"),
    );
    write_file(
        root,
        "node_modules/reexport-chain-pkg/leaf.js",
        "exports.leafName = \"leaf-value\";\n",
    );
    // c) a <-> b cycle, reached through the package entry (so the ESM wrapper
    // targets entry.js, not a cycle member — see the note on the loader-level
    // test): both sides contribute, the walk terminates.
    write_file(
        root,
        "node_modules/reexport-cycle-pkg/package.json",
        r#"{ "name": "reexport-cycle-pkg", "main": "./entry.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-cycle-pkg/entry.js",
        &format!("{TS_HELPER_PRELUDE}__exportStar(require(\"./a\"), exports);\n"),
    );
    write_file(
        root,
        "node_modules/reexport-cycle-pkg/a.js",
        &format!(
            "{TS_HELPER_PRELUDE}__exportStar(require(\"./b\"), exports);\nexports.fromA = \"a\";\n"
        ),
    );
    write_file(
        root,
        "node_modules/reexport-cycle-pkg/b.js",
        &format!(
            "{TS_HELPER_PRELUDE}__exportStar(require(\"./a\"), exports);\nexports.fromB = \"b\";\n"
        ),
    );
    // d) The direct `__createBinding(exports, m, "<name>"[, "<alias>"])` form.
    write_file(
        root,
        "node_modules/reexport-bind-pkg/package.json",
        r#"{ "name": "reexport-bind-pkg", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-bind-pkg/index.js",
        &format!(
            "{TS_HELPER_PRELUDE}var m = require(\"reexport-source-pkg\");\n\
             __createBinding(exports, m, \"alpha\", \"alphaAlias\");\n\
             __createBinding(exports, m, \"beta\");\n"
        ),
    );
    // e) Sources that resolve nowhere or are not require bindings: no names.
    write_file(
        root,
        "node_modules/reexport-missing-pkg/package.json",
        r#"{ "name": "reexport-missing-pkg", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-missing-pkg/index.js",
        &format!(
            "{TS_HELPER_PRELUDE}__exportStar(require(\"no-such-pkg-xyz\"), exports);\n\
             __exportStar(notARequireBinding, exports);\n\
             __createBinding(exports, notBoundEither, dynamicName);\n\
             exports.own = \"own-value\";\n"
        ),
    );
    // f) UMD wrapper: the factory body shares the module's `exports` object and
    // declares the surface with a chained assignment. The trailing forEach is a
    // plain callback that merely names a parameter `exports` — it must not be
    // mistaken for a factory.
    write_file(
        root,
        "node_modules/reexport-umd-pkg/package.json",
        r#"{ "name": "reexport-umd-pkg", "main": "./index.js" }"#,
    );
    write_file(
        root,
        "node_modules/reexport-umd-pkg/index.js",
        "(function (factory) {\n\
         \x20 if (typeof module === \"object\" && typeof module.exports === \"object\") {\n\
         \x20   var v = factory(require, exports);\n\
         \x20   if (v !== undefined) module.exports = v;\n\
         \x20 }\n\
         })(function (require, exports) {\n\
         \x20 \"use strict\";\n\
         \x20 Object.defineProperty(exports, \"__esModule\", { value: true });\n\
         \x20 exports.UmdAlpha = exports.UmdBeta = void 0;\n\
         \x20 var UmdAlpha = function () { return \"umd-alpha\"; };\n\
         \x20 exports.UmdAlpha = UmdAlpha;\n\
         });\n\
         [1].forEach(function (exports) { exports.notAnExport = 1; });\n",
    );
}

/// Pure lexer: `__exportStar` (literal + ident-bound), `__createBinding`, the
/// star-export cycle, unresolvable/dynamic sources, and the UMD factory body.
#[test]
fn cjs_lexer_follows_ts_reexport_helpers_and_umd_factories() {
    let root = fixture_root("cjs-reexport-forms");
    write_reexport_tree(&root);

    // a) `__exportStar(require("<bare>"), exports)`: the whole surface of the
    // target package, resolved through the node_modules walk-up.
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-star-pkg/index.js")),
        name_set(&["alpha", "beta"]),
        "bare __exportStar must bring in the target's names — and neither the \
         helper definitions nor __esModule"
    );

    // b) Two-level chain, the outer hop ident-bound to `require("./mid")`.
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-chain-pkg/index.js")),
        name_set(&["leafName"]),
        "ident-bound __exportStar must be traced through require bindings"
    );

    // c) a <-> b: both sides' names, no runaway recursion.
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-cycle-pkg/a.js")),
        name_set(&["fromA", "fromB"]),
        "star-export cycles terminate and union"
    );
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-cycle-pkg/entry.js")),
        name_set(&["fromA", "fromB"]),
        "the cycle is reached through the entry's star re-export too"
    );

    // d) Direct `__createBinding(exports, m, "name"[, "alias"])`.
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-bind-pkg/index.js")),
        name_set(&["alphaAlias", "beta"]),
        "the fourth argument is the exported alias, the third the source name"
    );

    // e) Unresolvable / non-require sources contribute nothing.
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-missing-pkg/index.js")),
        name_set(&["own"]),
        "an unresolvable specifier or a non-require ident must add no names"
    );

    // f) UMD factory body + chained `exports.A = exports.B = void 0`.
    assert_eq!(
        named_exports(&root.join("node_modules/reexport-umd-pkg/index.js")),
        name_set(&["UmdAlpha", "UmdBeta"]),
        "the UMD factory's exports surface counts; a plain `exports` callback does not"
    );

    let _ = std::fs::remove_dir_all(&root);
}

const CJS_REEXPORT_EXT_TS: &str = r#"
import { alpha, beta } from "reexport-star-pkg";
import { leafName } from "reexport-chain-pkg";
import { fromA, fromB } from "reexport-cycle-pkg";
import { alphaAlias, beta as betaDirect } from "reexport-bind-pkg";
import { UmdAlpha, UmdBeta } from "reexport-umd-pkg";
import reexportDefault from "reexport-star-pkg";

export default (pi: any) => {
  const out: any = {};
  out.alpha = alpha;
  out.beta = beta;
  out.leafName = leafName;
  out.fromA = fromA;
  out.fromB = fromB;
  out.alphaAlias = alphaAlias;
  out.betaDirect = betaDirect;
  out.aliasMatchesSource = alphaAlias === alpha;
  out.umdAlpha = (UmdAlpha as any)();
  out.umdBetaKind = typeof UmdBeta;
  out.defaultAlphaIsNamed = (reexportDefault as any).alpha === alpha;
  (globalThis as any).__cjsReexport = out;
};
"#;

/// The same names through the loader: `import { … } from "<cjs pkg>"` links
/// against the wrapper's link-time snapshot and the values are the runtime
/// ones (including the `__createBinding` alias and the UMD factory surface).
///
/// Two deliberate exclusions:
/// - `reexport-missing-pkg` — its `require` of a nonexistent package throws
///   when the CJS module executes, which is why the "no names for an
///   unresolvable source" case is asserted on the lexer alone;
/// - the cycle package is entered through `entry.js` rather than `a.js`. An
///   ESM-imported file that is also a member of a require cycle is currently
///   loaded twice (the wrapper registers `__pirCjsLoad`'s key with forward
///   slashes while `require` resolves the same file with backslashes on
///   Windows), so the CJS body executes twice against two `module.exports`
///   objects. That is a pre-existing path-spelling defect in the CJS bridge,
///   unrelated to the named-export scan, and it makes any strict-mode
///   `exports.X = …` after a cross-instance `__exportStar` copy throw
///   "no setter for property". The lexer's cycle handling is covered by
///   `cjs_lexer_follows_ts_reexport_helpers_and_umd_factories` instead.
#[tokio::test(flavor = "current_thread")]
async fn cjs_reexported_names_link_through_the_interop_wrapper() {
    let root = fixture_root("cjs-reexport-link");
    write_reexport_tree(&root);
    write_file(&root, "ext/main.ts", CJS_REEXPORT_EXT_TS);
    let rt = ExtensionRuntime::new(test_config_at(&root))
        .await
        .expect("runtime construction");

    let result = rt.load(&[root.join("ext/main.ts")]).await.expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__cjsReexport ?? null)")
        .await
        .expect("read __cjsReexport");
    assert_eq!(v["alpha"], "alpha-value", "{v}");
    assert_eq!(v["beta"], "beta-value", "{v}");
    assert_eq!(v["leafName"], "leaf-value", "{v}");
    assert_eq!(v["fromA"], "a", "star cycle peer name: {v}");
    assert_eq!(v["fromB"], "b", "star cycle peer name: {v}");
    assert_eq!(v["alphaAlias"], "alpha-value", "__createBinding alias: {v}");
    assert_eq!(v["betaDirect"], "beta-value", "__createBinding name: {v}");
    assert_eq!(v["aliasMatchesSource"], true, "{v}");
    assert_eq!(v["umdAlpha"], "umd-alpha", "UMD factory surface: {v}");
    assert_eq!(
        v["umdBetaKind"], "undefined",
        "the chained declaration's placeholder is a real (undefined) binding: {v}"
    );
    assert_eq!(
        v["defaultAlphaIsNamed"], true,
        "default is module.exports and carries the same values: {v}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
