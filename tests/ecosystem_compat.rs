//! Ecosystem compatibility harness: proves the extension loader can ingest the
//! pi packages installed under `<agent-dir>/npm/node_modules` WITHOUT executing
//! any third-party code.
//!
//! For every installed package whose `package.json` carries `pi.extensions`,
//! the test runs the same pipeline the runtime uses during `load`:
//!
//! 1. discovery (`discover_extensions`),
//! 2. entry classification (`module_kind_and_source_for_test`, the thin public
//!    seam over the loader's private `module_kind_and_source`),
//! 3. transpilation of ESM entries (`extensions::transpile`),
//! 4. dependency scanning (`scan_dependencies`), relative-specifier probing
//!    through the loader's NodeNext candidates, and bare-specifier resolution
//!    through the loader's npm resolver.
//!
//! Unsupported dependencies (unknown `node:` builtins) and unresolved bare
//! npm specifiers are structured expected errors, but only when the exact
//! `(package, specifier)` pair is listed in [`EXPECTED_LIMITATIONS`] with a
//! reason; an undocumented one fails the matrix. Unresolved *relative*
//! imports always fail (a broken install, not a platform limitation).
//!
//! Nothing is evaluated: no `ExtensionRuntime` is constructed and no
//! `ExtensionHandle::load`/`emit` runs. The npm tree is user-owned and may be
//! absent (CI images usually have none): the matrix then prints a skip line and
//! passes. Set `PIR_REQUIRE_INSTALLED_EXTENSIONS=1` to require the tree.
//! The whole matrix is bounded by [`MATRIX_BUDGET`] and deterministic:
//! packages, manifest entries and discovered entry paths are all sorted, and
//! every line is prefixed with `[ecosystem_compat]` for `--nocapture` runs.
//!
//! Run with:
//! `cargo test --features phone --test ecosystem_compat -- --nocapture`

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pir::extensions::api::shims::canonical_virtual_name;
use pir::extensions::loader::npm_resolve::{module_kind, resolve_bare, ModuleKind, ResolveMode};
use pir::extensions::loader::{
    absolute_path, module_kind_and_source_for_test, path_specifier_to_local,
    resolve_relative_for_test,
};
use pir::extensions::{
    discover_extensions, scan_dependencies, transpile, DependencyKind, DependencyRef,
};
use serde_json::Value;

/// Wall-clock budget for the whole installed-package matrix. The harness is
/// pure CPU over user-owned files (no code execution, no network); 60 s is
/// roughly 100x the observed runtime here and bounds a pathological tree
/// (e.g. a generated multi-hundred-MB entry) instead of hanging CI.
const MATRIX_BUDGET: Duration = Duration::from_secs(60);

/// Documented ingestion limitations, keyed by `(package, specifier)`. A
/// dependency the runtime cannot serve (an unknown `node:` builtin, or a bare
/// npm package that does not resolve from the installed tree) is only accepted
/// when it appears here with a reason; any other occurrence fails the matrix
/// with the exact specifier. Keep this list explicit: a package silently
/// growing a new unsupported import must not turn into a passing run.
///
/// Empty today — every installed package's entry resolves every scanned
/// dependency. Add an entry (with the platform reason) only when a real
/// installed package cannot be ingested without node/npm emulation that pir
/// deliberately does not provide.
const EXPECTED_LIMITATIONS: &[(&str, &str, &str)] = &[
    // ("package-name", "./imported-or-builtin-specifier", "why it cannot load"),
];

// ---------------------------------------------------------------------------
// Installed-package inventory
// ---------------------------------------------------------------------------

/// The npm tree pir installs extension packages into.
fn npm_root() -> PathBuf {
    pir::coding_agent::context::agent_dir()
        .join("npm")
        .join("node_modules")
}

#[derive(Debug, Clone)]
struct InstalledPackage {
    name: String,
    version: String,
    root: PathBuf,
    /// Verbatim `pi.extensions` entries from the package manifest.
    entries: Vec<String>,
}

/// One package's relevant manifest data. `Ok(None)` means "not an installed
/// extension package": no manifest, no `pi` field, no `pi.extensions` field,
/// or an empty `pi.extensions` array (upstream treats an empty manifest as
/// absent and falls through to the directory index rules, so a package only
/// enters this matrix when the manifest explicitly names entries). An
/// unreadable or malformed manifest is an `Err`, never a silent skip.
fn read_package_manifest(path: &Path) -> Result<Option<(String, String, Vec<String>)>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
    let Some(extensions) = json.get("pi").and_then(|pi| pi.get("extensions")) else {
        return Ok(None);
    };
    let entries: Vec<String> = extensions
        .as_array()
        .ok_or_else(|| format!("{}.pi.extensions is not an array", path.display()))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{}.pi.extensions has a non-string entry", path.display()))
        })
        .collect::<Result<_, _>>()?;
    if entries.is_empty() {
        return Ok(None);
    }
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} has no string \"name\"", path.display()))?
        .to_string();
    let version = json
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    Ok(Some((name, version, entries)))
}

/// The inventory of one npm tree: the extension packages plus explicit counts
/// of everything that was deliberately not treated as one, and the manifests
/// that could not be interpreted (reported, never silently dropped).
#[derive(Debug, Default)]
struct Inventory {
    packages: Vec<InstalledPackage>,
    /// Package directories with a readable manifest but no non-empty
    /// `pi.extensions` manifest. These are ordinary dependencies.
    non_extension_packages: usize,
    /// `package.json` files that exist but cannot be read/interpreted.
    problems: Vec<String>,
}

/// Walk the top level of the npm tree (plus one level of `@scope/` package
/// dirs). Nested `node_modules` are dependencies, not installed extensions.
fn installed_packages(root: &Path) -> Inventory {
    let mut inventory = Inventory::default();
    let mut package_dirs: Vec<PathBuf> = Vec::new();
    let Ok(read) = std::fs::read_dir(root) else {
        return inventory;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !path.is_dir() || name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') {
            if let Ok(scoped) = std::fs::read_dir(&path) {
                for child in scoped.flatten() {
                    if child.path().is_dir() {
                        package_dirs.push(child.path());
                    }
                }
            }
        } else {
            package_dirs.push(path);
        }
    }
    package_dirs.sort();

    for dir in package_dirs {
        match read_package_manifest(&dir.join("package.json")) {
            Ok(Some((name, version, entries))) => inventory.packages.push(InstalledPackage {
                name,
                version,
                root: dir,
                entries,
            }),
            Ok(None) => inventory.non_extension_packages += 1,
            Err(problem) => inventory.problems.push(problem),
        }
    }
    inventory.packages.sort_by(|a, b| a.name.cmp(&b.name));
    inventory
}

// ---------------------------------------------------------------------------
// Per-entry probe (read-only; mirrors the runtime's entry seed)
// ---------------------------------------------------------------------------

/// One structured expected error: the runtime will reject this specifier at
/// load time. The package name is attached by the matrix, which accepts the
/// limitation only when [`EXPECTED_LIMITATIONS`] documents it.
#[derive(Debug)]
struct IngestionLimitation {
    specifier: String,
    detail: String,
}

#[derive(Debug)]
struct EntryReport {
    kind: ModuleKind,
    source_bytes: usize,
    output_bytes: usize,
    /// Whether the source went through the parse/print transpiler (as opposed
    /// to the verbatim plain-JS fast path).
    parsed: bool,
    deps: Vec<DependencyRef>,
    problems: Vec<String>,
    /// Structured expected errors: the runtime is expected to reject these at
    /// load time. They are printed and counted, never hidden and never treated
    /// as a clean ingest.
    limitations: Vec<IngestionLimitation>,
}

fn probe_entry(path: &Path) -> Result<EntryReport, String> {
    let (kind, classified) = module_kind_and_source_for_test(path);
    let source = match classified {
        // The classifier already read an ambiguous `.js` to decide ESM/CJS;
        // reuse that text exactly like the runtime seed does.
        Some(source) => source,
        None => {
            std::fs::read_to_string(path).map_err(|error| format!("cannot read source: {error}"))?
        }
    };

    let mut report = EntryReport {
        kind,
        source_bytes: source.len(),
        output_bytes: source.len(),
        parsed: false,
        deps: Vec::new(),
        problems: Vec::new(),
        limitations: Vec::new(),
    };

    let javascript = match kind {
        // The runtime wraps CJS/JSON through their bridges; the scan still
        // runs over that text. Neither goes through the TypeScript parser.
        ModuleKind::Cjs | ModuleKind::Json => source.clone(),
        ModuleKind::Native => {
            return Err("native addon (.node) entries cannot be ingested".to_string());
        }
        ModuleKind::Esm => {
            let output = transpile(&path.to_string_lossy(), &source)
                .map_err(|error| format!("transpile failed: {error}"))?;
            report.parsed = output != source;
            output
        }
    };

    report.output_bytes = javascript.len();
    report.deps = scan_dependencies(&javascript);

    // The transpiler promises that TypeScript-only syntax is gone from files it
    // reparsed. Assert it on the emitted text, not on the input: this is what
    // QuickJS will parse.
    if report.parsed {
        let surviving = surviving_ts_shapes(&javascript);
        if !surviving.is_empty() {
            report.problems.push(format!(
                "TypeScript declarations survived transpilation: {surviving:?}"
            ));
        }
        let ts_specifiers: Vec<String> = report
            .deps
            .iter()
            .filter_map(|dep| ts_import_specifier(&dep.specifier))
            .collect();
        if !ts_specifiers.is_empty() {
            report.problems.push(format!(
                "TypeScript import specifiers survived transpilation: {ts_specifiers:?}"
            ));
        }
    }

    // Dependency support is reported in two honest buckets:
    //
    // * `problems` — the scan/transpile contract was violated (surviving TS,
    //   malformed output), a real failure.
    // * `limitations` — a structured *expected* error: the runtime itself
    //   rejects this specifier at load time. Unknown `node:` builtins are the
    //   runtime's documented rejection (`DependencyRef::is_supported`); a bare
    //   npm package that does not resolve from this install is reported too
    //   instead of being silently accepted by the scan. Both are printed as
    //   EXPECTED lines so a package that cannot be fully ingested is named,
    //   never skipped.
    for dep in &report.deps {
        if !dep.is_supported() {
            report.limitations.push(IngestionLimitation {
                specifier: dep.specifier.clone(),
                detail: format!(
                    "unsupported dependency {} ({:?}): the runtime rejects this module at load time",
                    dep.specifier, dep.kind
                ),
            });
            continue;
        }
        if is_bare_npm_specifier(&dep.specifier)
            && canonical_virtual_name(&dep.specifier).is_none()
            && !bare_specifier_resolves(path, &dep.specifier, dep.kind)
        {
            report.limitations.push(IngestionLimitation {
                specifier: dep.specifier.clone(),
                detail: format!(
                    "bare specifier {} ({:?}) does not resolve from the package tree; the runtime would fail this import at load time (no node/npm installer support in this harness)",
                    dep.specifier, dep.kind
                ),
            });
            continue;
        }
        // A relative import must exist under the runtime's own NodeNext
        // probing. A missing file is a real ingestion failure (the package was
        // installed broken), not an expected limitation.
        if dep.specifier.starts_with('.')
            && resolve_relative_for_test(path, &dep.specifier).is_none()
        {
            report.problems.push(format!(
                "relative import {} ({:?}) does not resolve from {} under the loader's NodeNext rules",
                dep.specifier,
                dep.kind,
                path.display()
            ));
        }
    }

    Ok(report)
}

/// A bare npm package specifier (`chalk`, `@scope/pkg/sub`) as opposed to a
/// relative/absolute/`file:` path, a `#` package import, or a `node:` builtin.
fn is_bare_npm_specifier(specifier: &str) -> bool {
    !specifier.starts_with('.')
        && !specifier.starts_with('#')
        && !specifier.starts_with("node:")
        && path_specifier_to_local(specifier).is_none()
}

/// Does a bare specifier resolve on disk through the loader's own npm
/// resolution (walk-up `node_modules` from the importing file + `"exports"`)?
/// Read-only; this is the same function the runtime resolver uses.
fn bare_specifier_resolves(importer: &Path, specifier: &str, kind: DependencyKind) -> bool {
    let dir = importer.parent().unwrap_or(importer);
    let mode = match kind {
        DependencyKind::Require => ResolveMode::Require,
        DependencyKind::StaticImport | DependencyKind::DynamicImport => ResolveMode::Esm,
    };
    resolve_bare(dir, specifier, mode).is_ok()
}

/// Top-level TypeScript declarations that must not survive type stripping.
/// The reprint puts every top-level statement at column zero, so indented
/// lines (function bodies, object literals) are skipped.
fn surviving_ts_shapes(javascript: &str) -> Vec<String> {
    const PREFIXES: &[&str] = &[
        "interface ",
        "type ",
        "enum ",
        "namespace ",
        "declare ",
        "abstract class ",
        "import type ",
        "export type ",
        "export interface ",
        "export enum ",
        "export namespace ",
        "export declare ",
        "export abstract class ",
    ];
    let mut shapes = Vec::new();
    for line in javascript.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        if PREFIXES.iter().any(|prefix| line.starts_with(prefix)) {
            shapes.push(line.chars().take(100).collect());
        }
    }
    shapes
}

/// A relative specifier still spelled as a TypeScript file. The transpiler
/// reprints modules but does not rewrite import specifiers; TypeScript
/// extensions are expected to import their NodeNext `.js` spelling instead.
fn ts_import_specifier(specifier: &str) -> Option<String> {
    let lower = specifier.to_ascii_lowercase();
    let spelled = [".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|suffix| lower.ends_with(suffix));
    spelled.then(|| specifier.to_string())
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// The installed-package matrix
// ---------------------------------------------------------------------------

#[test]
fn installed_pi_packages_are_ingestible() {
    let root = npm_root();
    if !root.is_dir() {
        eprintln!(
            "[ecosystem_compat] npm root {} does not exist; skipping installed-package matrix",
            root.display()
        );
        assert!(
            std::env::var_os("PIR_REQUIRE_INSTALLED_EXTENSIONS").is_none(),
            "PIR_REQUIRE_INSTALLED_EXTENSIONS is set but {} does not exist",
            root.display()
        );
        return;
    }

    let started = Instant::now();
    let inventory = installed_packages(&root);
    if inventory.packages.is_empty() {
        eprintln!(
            "[ecosystem_compat] no packages with pi.extensions under {}; skipping \
             ({} dependency package(s) without the manifest, {} manifest problem(s))",
            root.display(),
            inventory.non_extension_packages,
            inventory.problems.len()
        );
        for problem in &inventory.problems {
            eprintln!("[ecosystem_compat] FAIL inventory: {problem}");
        }
        assert!(
            inventory.problems.is_empty(),
            "cannot interpret {} package manifest(s) under {}; see the FAIL lines above",
            inventory.problems.len(),
            root.display()
        );
        assert!(
            std::env::var_os("PIR_REQUIRE_INSTALLED_EXTENSIONS").is_none(),
            "PIR_REQUIRE_INSTALLED_EXTENSIONS is set but no package with pi.extensions \
             was found under {}",
            root.display()
        );
        return;
    }

    eprintln!("[ecosystem_compat] npm root: {}", root.display());
    eprintln!(
        "[ecosystem_compat] {} package(s) with pi.extensions; {} dependency package(s) \
         without the manifest; {} manifest problem(s)",
        inventory.packages.len(),
        inventory.non_extension_packages,
        inventory.problems.len()
    );

    let mut failures: Vec<String> = inventory
        .problems
        .iter()
        .map(|problem| format!("inventory: {problem}"))
        .collect();
    let mut total_entries = 0usize;
    let mut ingested_entries = 0usize;
    let mut total_limitations = 0usize;

    for package in &inventory.packages {
        let mut package_failures: Vec<String> = Vec::new();
        let mut package_limitations: Vec<String> = Vec::new();
        let mut package_entries = 0usize;
        eprintln!(
            "[ecosystem_compat] package {}@{} ({} manifest entr{})",
            package.name,
            package.version,
            package.entries.len(),
            if package.entries.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );

        // (1) Discovery from the package root: the manifest-driven path the
        // runtime takes for an explicit `-e <package-dir>` (and the path the
        // package manager's collected extension roots reach).
        let discovery = discover_extensions(std::slice::from_ref(&package.root));
        for error in &discovery.errors {
            package_failures.push(format!(
                "{}: discovery error for {}: {}",
                package.name,
                error.path.display(),
                error.error
            ));
        }

        if discovery.extensions.is_empty() {
            package_failures.push(format!(
                "{}: discovery found no loadable entry (manifest: {:?})",
                package.name, package.entries
            ));
        } else {
            // Every declared manifest target must be represented. A target may
            // be a directory (pi-web-access `./dist`, pi-codegraph
            // `./extensions`): run the same directory discovery on it and
            // require its entries in the package-root result.
            for entry in &package.entries {
                let target = package.root.join(entry);
                if !target.exists() {
                    package_failures.push(format!(
                        "{}: manifest entry {entry} does not exist",
                        package.name
                    ));
                    continue;
                }
                let target_entries: Vec<PathBuf> = if target.is_file() {
                    // Discovery anchors every entry path; anchor the manifest
                    // target the same way before comparing.
                    vec![absolute_path(&target)]
                } else {
                    let sub = discover_extensions(std::slice::from_ref(&target));
                    if sub.extensions.is_empty() {
                        package_failures.push(format!(
                            "{}: manifest entry {entry} is a directory with no discoverable entry",
                            package.name
                        ));
                    }
                    sub.extensions
                        .into_iter()
                        .map(|extension| extension.path)
                        .collect()
                };
                for target_entry in target_entries {
                    if !discovery
                        .extensions
                        .iter()
                        .any(|extension| extension.path == target_entry)
                    {
                        package_failures.push(format!(
                            "{}: manifest entry {entry} resolved to {} but package-root discovery missed it",
                            package.name,
                            target_entry.display()
                        ));
                    }
                }
            }

            // (2)-(4) classification + transpile + dependency scan per entry.
            let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
            for extension in &discovery.extensions {
                if !seen.insert(extension.path.clone()) {
                    continue;
                }
                total_entries += 1;
                package_entries += 1;
                let relative = extension
                    .path
                    .strip_prefix(&package.root)
                    .unwrap_or(&extension.path);
                let probe = catch_unwind(AssertUnwindSafe(|| probe_entry(&extension.path)));
                match probe {
                    Ok(Ok(report)) => {
                        ingested_entries += 1;
                        eprintln!(
                            "[ecosystem_compat]   entry {} kind={:?} source={}B transpiled={}B parsed={} deps={} limitations={}",
                            relative.display(),
                            report.kind,
                            report.source_bytes,
                            report.output_bytes,
                            report.parsed,
                            report.deps.len(),
                            report.limitations.len()
                        );
                        for limitation in report.limitations {
                            let documented = EXPECTED_LIMITATIONS.iter().find(|(name, spec, _)| {
                                *name == package.name.as_str() && *spec == limitation.specifier
                            });
                            let line = format!(
                                "{}: {}: {}",
                                package.name,
                                relative.display(),
                                limitation.detail
                            );
                            match documented {
                                Some((_, _, reason)) => {
                                    package_limitations
                                        .push(format!("{line} [documented: {reason}]"));
                                }
                                None => package_failures.push(format!(
                                    "{line} (unexpected limitation: add an explicit entry to \
                                     EXPECTED_LIMITATIONS with the platform reason if this \
                                     package genuinely cannot be ingested)"
                                )),
                            }
                        }
                        for problem in report.problems {
                            package_failures.push(format!(
                                "{}: {}: {}",
                                package.name,
                                relative.display(),
                                problem
                            ));
                        }
                    }
                    Ok(Err(error)) => {
                        package_failures.push(format!(
                            "{}: {}: {}",
                            package.name,
                            relative.display(),
                            error
                        ));
                        eprintln!(
                            "[ecosystem_compat]   entry {} FAILED: {}",
                            relative.display(),
                            error
                        );
                    }
                    Err(payload) => {
                        let message = panic_message(payload.as_ref());
                        package_failures.push(format!(
                            "{}: {}: PANIC: {}",
                            package.name,
                            relative.display(),
                            message
                        ));
                        eprintln!(
                            "[ecosystem_compat]   entry {} PANICKED: {}",
                            relative.display(),
                            message
                        );
                    }
                }
            }
        }

        let status = if package_failures.is_empty() {
            "PASS"
        } else {
            "FAIL"
        };
        eprintln!(
            "[ecosystem_compat] package {}: {status} entries={package_entries} failures={} expected_limitations={}",
            package.name,
            package_failures.len(),
            package_limitations.len()
        );
        for limitation in &package_limitations {
            eprintln!("[ecosystem_compat]   EXPECTED LIMITATION {limitation}");
        }
        total_limitations += package_limitations.len();
        failures.append(&mut package_failures);
    }

    let elapsed = started.elapsed();
    eprintln!(
        "[ecosystem_compat] summary: packages={} entries={} ingested={} expected_limitations={} failed={} elapsed_ms={}",
        inventory.packages.len(),
        total_entries,
        ingested_entries,
        total_limitations,
        failures.len(),
        elapsed.as_millis()
    );
    for failure in &failures {
        eprintln!("[ecosystem_compat] FAIL {failure}");
    }
    assert!(
        failures.is_empty(),
        "{} ecosystem ingestion failure(s); see the [ecosystem_compat] lines above",
        failures.len()
    );
    assert!(
        elapsed < MATRIX_BUDGET,
        "installed-package matrix took {:?} (budget {:?}); the harness must stay bounded",
        elapsed,
        MATRIX_BUDGET
    );
}

// ---------------------------------------------------------------------------
// Transpiler contract probes (generic shapes, no third-party files)
// ---------------------------------------------------------------------------

/// `import type` / `export type` / `interface` / type aliases are erased, while
/// runtime imports survive.
#[test]
fn transpiler_strips_type_only_constructs() {
    let source = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { type RuntimeThing, runtimeValue } from "./dep.js";
export type { ReExported } from "./dep.js";
interface Probe { value: number }
type Alias = Probe;
export const value: Alias = runtimeValue;
export default function (_pi: ExtensionAPI): void {}
"#;
    let javascript = transpile("contract-types.ts", source).expect("type-only syntax must strip");
    assert!(
        surviving_ts_shapes(&javascript).is_empty(),
        "TypeScript declarations survived:\n{javascript}"
    );
    let deps = scan_dependencies(&javascript);
    assert!(
        deps.iter().any(|dep| dep.specifier == "./dep.js"),
        "runtime import must survive: {deps:?}"
    );
    assert!(
        !deps
            .iter()
            .any(|dep| dep.specifier == "@earendil-works/pi-coding-agent"),
        "type-only import must be erased: {deps:?}"
    );
}

/// Mixed value/type specifiers, `import type * as`, `export type {}` and
/// `export type *` erase only the type half: the value import (and its
/// specifier) survives, the type-only module reference disappears.
#[test]
fn transpiler_strips_mixed_and_star_type_imports() {
    let source = r#"
import value, { type Shape, other } from "./dep.js";
import type * as Types from "./types.js";
export type { Shape } from "./dep.js";
export type * from "./types.js";
export { type Hidden, other as forwarded } from "./dep.js";
export const result: Types.Box<Shape> = { value, other };
"#;
    let javascript =
        transpile("contract-mixed-types.ts", source).expect("mixed type imports must strip");
    assert!(
        surviving_ts_shapes(&javascript).is_empty(),
        "TypeScript declarations survived:\n{javascript}"
    );
    for erased in ["Shape", "Hidden", "Types."] {
        assert!(
            !javascript.contains(erased),
            "type-only binding {erased} survived:\n{javascript}"
        );
    }
    let deps = scan_dependencies(&javascript);
    assert!(
        deps.iter().any(|dep| dep.specifier == "./dep.js"),
        "the value import/export must survive: {deps:?}"
    );
    assert!(
        !deps.iter().any(|dep| dep.specifier == "./types.js"),
        "an all-type `import type *` / `export type *` must erase its specifier: {deps:?}"
    );
}

/// `.js/.mjs/.cjs` are plain JavaScript by definition (Node parity): TypeScript
/// syntax in a `.js` entry reaches the engine verbatim and is a runtime syntax
/// error there, never silently type-stripped. This is the documented boundary
/// of "TS in .js" — a `.ts`/`.mts`/`.cts` spelling is required for stripping.
#[test]
fn plain_js_with_ts_syntax_is_verbatim_node_parity() {
    let source =
        "// comment and spacing preserved\nconst value: number = 1;\n\nexport default value;\n";
    assert_eq!(
        transpile("contract-ts-in-js.js", source).expect("the .js fast path never parses"),
        source,
        "a .js entry must not be type-stripped"
    );
    let module_source = "export const value: number = 1;\n";
    assert_eq!(
        transpile("contract-ts-in-mjs.mjs", module_source).unwrap(),
        module_source
    );
}

/// Ambient declarations and class-only TypeScript modifiers are erased, and
/// parameter properties are lowered to assignments (runtime semantics that a
/// naive "delete type annotations" pass would drop).
#[test]
fn transpiler_strips_ambient_and_class_type_syntax() {
    let source = r#"
import type DefaultType from "./dep.js";
import type { A as B } from "./dep.js";
export type * as Types from "./types.js";
export default interface Shape { value: number }
declare global { interface Window { probe: number } }
declare module "virtual" { export const x: number }
abstract class Base {
  constructor(private readonly value: number, public label?: string) {}
  abstract run(): void;
  declare extensionOnly: string;
}
const enum Mode { Fast = 1, Slow = 2 }
export class Impl extends Base {
  run(): void {}
}
export const mode: Mode = Mode.Fast;
"#;
    let javascript =
        transpile("contract-ambient.ts", source).expect("ambient TypeScript must strip");
    assert!(
        surviving_ts_shapes(&javascript).is_empty(),
        "TypeScript declarations survived:\n{javascript}"
    );
    for erased in [
        "DefaultType",
        "interface",
        "declare",
        "abstract",
        "readonly",
        "public",
        "const enum",
        "extensionOnly",
    ] {
        assert!(
            !javascript.contains(erased),
            "`{erased}` survived type stripping:\n{javascript}"
        );
    }
    assert!(
        javascript.contains("this.value = value"),
        "parameter properties must be lowered to assignments:\n{javascript}"
    );
    let deps = scan_dependencies(&javascript);
    assert!(
        !deps.iter().any(|dep| dep.specifier == "./types.js"),
        "`export type * as` must erase its specifier: {deps:?}"
    );
}

/// Runtime TypeScript constructs are lowered, not just parsed.
#[test]
fn transpiler_lowers_enums_namespaces_satisfies_and_const_assertions() {
    let source = r#"
enum Color { Red = 1, Blue = Red + 1 }
namespace Palette {
  export const primary = Color.Red;
}
const config = { mode: "fast", color: Palette.primary } as const satisfies {
  mode: string;
  color: Color;
};
export default config;
"#;
    let javascript =
        transpile("contract-lowering.ts", source).expect("lowerable TS must transpile");
    assert!(
        surviving_ts_shapes(&javascript).is_empty(),
        "TypeScript declarations survived:\n{javascript}"
    );
    assert!(
        !javascript.contains("satisfies") && !javascript.contains(" as const"),
        "satisfies/as-const must be erased:\n{javascript}"
    );
    assert!(
        javascript.contains("Color") && javascript.contains("Palette"),
        "enum/namespace runtime bindings must be emitted:\n{javascript}"
    );
}

/// Shebang, BOM and CRLF are all accepted; the emitted module text is clean.
#[test]
fn transpiler_handles_shebang_bom_and_crlf() {
    let with_shebang = transpile(
        "contract-shebang.ts",
        "#!/usr/bin/env node\nexport const x = 1;\n",
    )
    .expect("shebang must parse");
    assert!(
        with_shebang.starts_with("#!/usr/bin/env node"),
        "shebang must be preserved:\n{with_shebang}"
    );

    let with_bom = transpile("contract-bom.js", "\u{feff}export const x = 1;\n")
        .expect("BOM must be accepted");
    assert!(
        !with_bom.starts_with('\u{feff}'),
        "BOM leaked into the emitted module"
    );
    assert!(with_bom.contains("export const x = 1;"));

    // BOM + shebang: the BOM must not hide the shebang from the plain-JS
    // shortcut (the transpiler path owns shebang handling).
    let bom_shebang = transpile(
        "contract-bom-shebang.js",
        "\u{feff}#!/usr/bin/env node\nexport const x = 1;\n",
    )
    .expect("BOM + shebang must parse");
    assert!(
        bom_shebang.starts_with("#!/usr/bin/env node"),
        "BOM hid the shebang:\n{bom_shebang}"
    );

    // TS syntax forces the reprint path, which emits LF-only output.
    let crlf = transpile(
        "contract-crlf.ts",
        "export const x: number = 1;\r\nexport const y: number = 2;\r\n",
    )
    .expect("CRLF must parse");
    assert!(!crlf.contains('\r'), "CRLF survived the reprint:\n{crlf:?}");
    assert!(crlf.contains("export const x = 1;"));
}

/// JSON imports and dynamic imports are classified, including the bundled
/// `import(/* comment */ "…")` shape.
#[test]
fn dependency_scan_covers_json_and_commented_dynamic_imports() {
    let source = r#"
import data from "./data.json" with { type: "json" };
export { helper } from "./helper.js";
export default async function load() {
  const lazy = await import(/* webpackChunkName: "lazy" */ "./lazy.js");
  const nested = await import(
    // line comment between the paren and the specifier
    "./nested.js"
  );
  return [data, helper, lazy.default, nested.default];
}
"#;
    let deps = scan_dependencies(source);
    let find = |specifier: &str| {
        deps.iter()
            .find(|dep| dep.specifier == specifier)
            .unwrap_or_else(|| panic!("missing {specifier}: {deps:?}"))
    };
    assert_eq!(find("./data.json").kind, DependencyKind::StaticImport);
    assert_eq!(find("./helper.js").kind, DependencyKind::StaticImport);
    assert_eq!(find("./lazy.js").kind, DependencyKind::DynamicImport);
    assert_eq!(find("./nested.js").kind, DependencyKind::DynamicImport);
}

/// `.mjs`/`.cjs`/`.mts`/`.cts` classification and the verbatim JS fast path.
#[test]
fn module_kind_covers_node_and_typescript_module_spellings() {
    assert_eq!(module_kind(Path::new("probe.mjs")), ModuleKind::Esm);
    assert_eq!(module_kind(Path::new("probe.cjs")), ModuleKind::Cjs);
    assert_eq!(module_kind(Path::new("probe.mts")), ModuleKind::Esm);
    assert_eq!(module_kind(Path::new("probe.cts")), ModuleKind::Cjs);
    assert_eq!(module_kind(Path::new("probe.json")), ModuleKind::Json);
    assert_eq!(module_kind(Path::new("probe.node")), ModuleKind::Native);

    let plain = "export const value = 1;\n";
    assert_eq!(
        transpile("probe.mjs", plain).expect("plain .mjs"),
        plain,
        "plain JS modules keep their exact text"
    );
    assert_eq!(
        transpile("probe.cjs", "module.exports = 1;\n").expect("plain .cjs"),
        "module.exports = 1;\n"
    );
}

/// Ambiguous `.js` (no nearest `"type"`) is classified from its source text,
/// and that text is handed back to the caller for a single-read seed.
#[test]
fn ambiguous_js_is_classified_and_source_is_reused() {
    let dir = std::env::temp_dir().join(format!(
        "pir-ecosystem-compat-ambiguous-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    // The nearest manifest carries no `"type"`: that is the ambiguous shape,
    // and owning the manifest keeps the walk from reaching a real package
    // scope above the temp directory.
    std::fs::write(dir.join("package.json"), "{\"name\":\"ambiguous-probe\"}\n")
        .expect("write ambiguous manifest");

    let esm = dir.join("esm.js");
    std::fs::write(&esm, "export const value = 1;\n").expect("write esm");
    let (kind, source) = module_kind_and_source_for_test(&esm);
    assert_eq!(kind, ModuleKind::Esm);
    assert_eq!(source.as_deref(), Some("export const value = 1;\n"));

    let cjs = dir.join("cjs.js");
    std::fs::write(&cjs, "module.exports = 1;\n").expect("write cjs");
    let (kind, source) = module_kind_and_source_for_test(&cjs);
    assert_eq!(kind, ModuleKind::Cjs);
    assert_eq!(source.as_deref(), Some("module.exports = 1;\n"));

    // An explicit package type wins without reading the file (source None).
    std::fs::write(dir.join("package.json"), "{\"type\":\"module\"}\n").expect("write manifest");
    let (kind, source) = module_kind_and_source_for_test(&cjs);
    assert_eq!(kind, ModuleKind::Esm);
    assert!(source.is_none(), "typed .js must not be read for detection");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest may name a directory (`pi-web-access` → `./dist`,
/// `@vndv/pi-codegraph` → `./extensions`); discovery expands it through the
/// directory rules instead of reporting a missing entry.
#[test]
fn manifest_directory_entries_expand_through_directory_rules() {
    let dir = std::env::temp_dir().join(format!(
        "pir-ecosystem-compat-manifest-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let package = dir.join("pkg");
    std::fs::create_dir_all(package.join("dist")).expect("dist dir");
    std::fs::create_dir_all(package.join("extensions")).expect("extensions dir");
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"probe","pi":{"extensions":["./dist","./extensions"]}}"#,
    )
    .expect("manifest");
    std::fs::write(
        package.join("dist").join("index.js"),
        "export default () => {};\n",
    )
    .expect("dist index");
    std::fs::write(
        package.join("extensions").join("probe.ts"),
        "export default () => {};\n",
    )
    .expect("extension file");

    let result = discover_extensions(std::slice::from_ref(&package));
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let mut names: Vec<String> = result
        .extensions
        .iter()
        .map(|extension| {
            extension
                .path
                .strip_prefix(&package)
                .unwrap_or(&extension.path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "dist/index.js".to_string(),
            "extensions/probe.ts".to_string()
        ]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Known unsupported shapes are explicit expected errors, never silent skips.
#[test]
fn known_unsupported_shapes_are_explicit_errors() {
    // Decorators: the embedded engine (QuickJS) has no decorator syntax and a
    // decorator has runtime semantics, so the transpiler rejects the file
    // instead of erasing behavior. Expected error, not a panic and not a
    // silent strip.
    let decorated = r#"
function sealed<T extends { new (...args: any[]): object }>(ctor: T): T {
  return ctor;
}
@sealed
class Probe {
  value = 1;
}
export default Probe;
"#;
    let error = transpile("contract-decorators.ts", decorated)
        .expect_err("decorators are a known unsupported shape");
    assert!(!error.is_empty(), "decorator rejection needs a message");

    // JSX: no JSX transform is linked (extension entries are .ts/.js), so a
    // `.tsx` entry containing JSX is a structured error rather than emitted
    // JSX that QuickJS cannot parse.
    let jsx = "export default () => <div>hello</div>;\n";
    assert!(
        transpile("contract-jsx.tsx", jsx).is_err(),
        "JSX without a linked JSX transform must be an explicit error"
    );
}
