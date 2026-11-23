//! Loader semantics parity with upstream
//! `packages/coding-agent/src/core/resource-loader.ts`:
//!
//! - `builtin:<name>` resource paths resolve against the builtin factory
//!   list ([`super::builtin`]); unknown names produce the upstream
//!   `Unknown built-in extension: <path>` error.
//! - `disabledBuiltinExtensions` removes `builtin:<name>` paths (the
//!   upstream `--no-mcp` → `["mcp"]` pattern) even when settings or `-e`
//!   enable them.
//! - `--no-extensions` keeps ONLY the CLI (`-e`) paths — an explicit `-e`
//!   still loads (upstream `noExtensions ? cliEnabledExtensions : merge`).
//! - Path ordering (upstream `package-manager.ts:986-1001` appends the
//!   builtin loop AFTER `resolveLocalEntries`/`addAutoDiscoveredResources`;
//!   `resource-loader.ts:608-617` merges CLI before discovered): CLI paths
//!   first, then settings/default paths with project-local before
//!   user-global, then the default-enabled factory builtins; the whole list
//!   dedups by canonical path; inline (Rust factory) plugins append last.
//!   Builtins therefore lose first-wins conflicts (tools/commands/renderer
//!   chains) to file extensions, exactly like upstream.
//! - Native-first `tool-repair` precedence ([`tool_repair_precedence`]): when
//!   the effective plan carries `builtin:tool-repair`, the JS
//!   `pi-tool-repair` package's entry is dropped from the plan and from the
//!   runtime load list, so only the native face runs; with the builtin
//!   disabled (removed by [`filter_disabled_builtins`]) or absent the JS
//!   entry stays — the fallback face.
//! - Default-disabled package policy ([`DEFAULT_DISABLED_PACKAGE_NAMES`]):
//!   packages that stay installed and listed in `settings.packages` but do
//!   not load unless the user opts in. `@malinamnam/pi-phone` is the one
//!   entry: it starts a local web server and costs ~14 MB of QuickJS heap on
//!   every startup. [`filter_default_disabled_packages`] drops its resolved
//!   extension paths BEFORE the runtime starts (a plan left with only
//!   builtins therefore never creates the QuickJS actor), and the host emits
//!   the returned skip as one `[ext]` diagnostic carrying the exact opt-in.
//!   Opt-in precedence: an explicit CLI `-e/--extension <path>` naming the
//!   package (or anything inside it) wins first; otherwise
//!   `defaultDisabledPackagesEnabled: true` in the GLOBAL settings scope
//!   turns the whole default-disabled set on. Project settings cannot enable
//!   it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::source::{builtin_name, is_builtin_path, BUILTIN_PATH_PREFIX};

/// Native-first precedence pairs: `(builtin name, JS package path component)`.
/// When the native builtin is on the effective plan the matching JS package
/// entry is dropped so only the native face runs (the CLR analogy: if the
/// native runtime can serve it, don't load the scripted copy).
///
/// All pairs are on by default: the Rust ports run with no plugin source
/// change and no user flag, which is the whole point of the native surface
/// (a scripted copy of the same package would just cost interpreted-JS
/// throughput). `PIR_PREFER_NATIVE=0` restores upstream first-wins for the
/// full-port pairs; the `tool-repair`/`phone` single-face rules stay always-on
/// because two copies running the same repair pipeline or phone server would
/// double-apply.
const NATIVE_PRECEDENCE: &[(&str, &str)] = &[
    ("tool-repair", "pi-tool-repair"),
    ("phone", "@malinamnam/pi-phone"),
    ("subagent", "pi-subagents"),
    ("mcp", "pi-mcp"),
    ("codemode", "pi-codemode"),
    ("tool-search", "pi-tool-search"),
    ("todo", "rpiv-todo"),
    ("context-mode", "context-mode"),
];

/// Whether the native-first set is active (default ON). The Rust ports replace
/// their unmodified JS packages automatically; `PIR_PREFER_NATIVE=0` (also
/// `false`/`off`) opts out and keeps upstream's first-wins behavior.
fn prefer_native_enabled() -> bool {
    !matches!(
        std::env::var("PIR_PREFER_NATIVE").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Whether the precedence table entry at `index` applies. The always-on
/// single-face rules (tool-repair, phone) ignore the preference switch.
/// single-face rules (tool-repair, phone) ignore the opt-in flag; the extra
/// full-port pairs need `PIR_PREFER_NATIVE`.
fn native_precedence_applies(index: usize, prefer_native: bool) -> bool {
    index < 2 || prefer_native
}

/// Whether `path` names JS package `package` (any normalized path component,
/// ASCII case-insensitive — npm `node_modules` layout or a checkout).
fn is_js_package_path(path: &Path, package: &str) -> bool {
    path.to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .any(|component| component.eq_ignore_ascii_case(package))
}

/// Apply the native-first table to an effective path list in place.
fn apply_native_precedence(paths: &mut Vec<PathBuf>) {
    for (index, (builtin, package)) in NATIVE_PRECEDENCE.iter().enumerate() {
        // Indices 0/1 are the always-on single-face rules; the rest need opt-in.
        if !native_precedence_applies(index, prefer_native_enabled()) {
            continue;
        }
        let builtin_on_plan = paths
            .iter()
            .any(|path| builtin_name(&path.to_string_lossy()) == Some(*builtin));
        if builtin_on_plan {
            if *builtin == "phone" {
                // The phone package resolves through `package.json` names and
                // the `npm:` install shape, not just path components (see
                // `default_disabled_package_name`). Keep that resolution.
                paths.retain(|path| default_disabled_package_name(path) != Some(*package));
            } else {
                paths.retain(|path| !is_js_package_path(path, package));
            }
        }
    }
}

/// Which `tool-repair` face an extension plan settles on — see
/// [`tool_repair_precedence`] for the rule and why the native builtin wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRepairPrecedence {
    /// The native `builtin:tool-repair` is on the effective plan: it is
    /// instantiated, and the JS `pi-tool-repair` entry is dropped from both
    /// the plan and the runtime load list.
    NativeBuiltin,
    /// The native builtin is unavailable (disabled via
    /// `disabledBuiltinExtensions`, or never on the plan): the JS package's
    /// entry stays — the fallback face, so the user never loses both.
    JsPackage,
    /// Neither face is on the plan.
    Absent,
}

/// Decide the native-vs-JS `tool-repair` precedence from the two effective
/// plan facts.
///
/// The native builtin wins whenever it is on the plan: it is the same repair
/// pipeline as the JS package's `message_end` handler, applied natively, and
/// only ONE `tool-repair` face may run — the JS copy is dropped so the same
/// message cannot pass through the repair pipeline twice. (The loop applies
/// the generic JS `message_end` return value since the parity lane; the JS
/// `pi-tool-repair` face therefore lands fine when it is the chosen one — the
/// suppression is a single-face determinism rule, not a workaround for a
/// dropped return.) The JS copy is kept back only when the builtin is
/// disabled (`builtin:tool-repair` removed from the plan by
/// [`filter_disabled_builtins`]) — otherwise a user with both configured
/// would get neither. `builtin_on_plan` is the EFFECTIVE plan's fact
/// (post-disabled-filter), not merely "the default builtins exist".
pub fn tool_repair_precedence(
    builtin_on_plan: bool,
    js_package_on_plan: bool,
) -> ToolRepairPrecedence {
    match (builtin_on_plan, js_package_on_plan) {
        (true, _) => ToolRepairPrecedence::NativeBuiltin,
        (false, true) => ToolRepairPrecedence::JsPackage,
        (false, false) => ToolRepairPrecedence::Absent,
    }
}

/// Split the resolved extension path list into builtin names (in order) and
/// on-disk file paths (in order, builtins removed so directory discovery
/// never sees a `builtin:` pseudo-path). The `tool-repair` precedence is
/// applied first, so a JS `pi-tool-repair` entry never reaches the runtime's
/// load list while the native builtin is on the plan (the one seam every host
/// load funnels through, `load_extension_runtime` included).
pub fn partition_paths(paths: &[PathBuf]) -> (Vec<String>, Vec<PathBuf>) {
    let mut effective = paths.to_vec();
    apply_native_precedence(&mut effective);
    let mut builtins = Vec::new();
    let mut files = Vec::new();
    for path in &effective {
        let text = path.to_string_lossy();
        if let Some(name) = builtin_name(&text) {
            builtins.push(name.to_owned());
        } else {
            files.push(path.clone());
        }
    }
    (builtins, files)
}

/// Remove `builtin:<name>` paths whose name is disabled (upstream
/// `disabledBuiltinExtensions` filter: "Built-in extensions not to load, by
/// name (such as `mcp`), even when settings or `-e` enable them"). The
/// `tool-repair` precedence then applies to the SURVIVING list: dropping
/// `builtin:tool-repair` here leaves the JS `pi-tool-repair` entry on the
/// plan (the fallback face), while any other disabled name still suppresses
/// the JS copy because the builtin survives.
pub fn filter_disabled_builtins(paths: &[PathBuf], disabled: &BTreeSet<String>) -> Vec<PathBuf> {
    let mut retained: Vec<PathBuf> = paths
        .iter()
        .filter(|path| {
            let text = path.to_string_lossy();
            match builtin_name(&text) {
                Some(name) => !disabled.contains(name),
                None => true,
            }
        })
        .cloned()
        .collect();
    apply_native_precedence(&mut retained);
    retained
}

/// Package names pir keeps DISABLED by default even when they are installed
/// and listed in `settings.packages`.
///
/// `@malinamnam/pi-phone` is a phone-first remote UI extension. Loading it
/// starts a local web server and costs about 14 MB of QuickJS heap for the
/// lifetime of the process — on every startup, whether or not the phone UI is
/// ever opened. It therefore stays a normal JS extension package (enabling it
/// restores the exact upstream JS behavior; there is no native
/// reimplementation) but is skipped by the loader unless the user opts in:
///
/// - `defaultDisabledPackagesEnabled: true` in the GLOBAL settings file, or
/// - an explicit CLI `-e/--extension <path>` naming the package (or any path
///   inside its checkout / installed directory) — explicit intent wins.
///
/// The package is matched by its resolved extension paths: the nearest
/// ancestor `package.json` `"name"` (checkout and
/// `<agentDir>/npm/node_modules/@malinamnam/pi-phone` alike) plus a
/// canonicalized path-component fallback for the npm layout, never by a
/// substring of a command line.
pub const DEFAULT_DISABLED_PACKAGE_NAMES: &[&str] = &["@malinamnam/pi-phone"];

/// One package skipped by [`filter_default_disabled_packages`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultDisabledPackageSkip {
    /// The matched package name from [`DEFAULT_DISABLED_PACKAGE_NAMES`].
    pub package_name: String,
    /// The first extension path dropped for this package.
    pub path: PathBuf,
    /// The diagnostic body (no `[ext]` prefix; the host emits it as
    /// `[ext] {message}`): why the package is off plus the exact opt-in.
    pub message: String,
}

/// The package name (from [`DEFAULT_DISABLED_PACKAGE_NAMES`]) whose package
/// root owns `path`, when `path` belongs to a default-disabled package.
///
/// Resolution order:
/// 1. the nearest ancestor `package.json` (the extension path itself when it
///    is a directory) whose `"name"` equals a default-disabled name — this is
///    what recognizes the `npm:@malinamnam/pi-phone[@version]` install shape,
///    a local checkout, and a symlinked `node_modules` entry alike;
/// 2. a canonicalized path-component match for the scoped npm name
///    (`node_modules/@malinamnam/pi-phone/...`), and a final component match
///    on the unscoped tail (`pi-phone`) for an unreadable manifest.
///
/// A readable nearest `package.json` naming some OTHER package is
/// authoritative: a renamed fork is not skipped.
pub fn default_disabled_package_name(path: &Path) -> Option<&'static str> {
    // Synthetic `builtin:<name>` / `<inline:...>` paths never belong to a
    // package (and must not make the ancestor walk read an unrelated
    // `package.json` from the process cwd).
    let text = path.to_string_lossy();
    if is_builtin_path(&text) || text.starts_with('<') {
        return None;
    }
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(manifest_name) = nearest_package_name(&resolved) {
        return DEFAULT_DISABLED_PACKAGE_NAMES
            .iter()
            .copied()
            .find(|name| name.eq_ignore_ascii_case(&manifest_name));
    }
    let components: Vec<String> = resolved
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    for name in DEFAULT_DISABLED_PACKAGE_NAMES {
        let parts: Vec<String> = name
            .split('/')
            .map(|part| part.to_ascii_lowercase())
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        if components
            .windows(parts.len())
            .any(|window| window == parts.as_slice())
        {
            return Some(name);
        }
        // Unscoped tail as a LAST resort (an unreadable package.json in a
        // checkout directory named `pi-phone`). Whole components only.
        if let Some(tail) = parts.last() {
            if components.iter().any(|component| component == tail) {
                return Some(name);
            }
        }
    }
    None
}

/// The `"name"` of the nearest `package.json` at or above `path`. Starting at
/// `path` itself when it is a directory, else at its parent. A found manifest
/// is authoritative (its absence of a name also stops the walk — a nested
/// package boundary must not borrow the parent's identity).
fn nearest_package_name(path: &Path) -> Option<String> {
    let mut current = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(dir) = current {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest).ok()?;
            let value: serde_json::Value = serde_json::from_str(&text).ok()?;
            return value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
        current = dir.parent();
    }
    None
}

/// Canonical comparison key for the explicit-CLI containment check:
/// canonicalized when possible, `/` separators, trailing separators removed,
/// lowercased on Windows.
fn comparable_key(path: &Path) -> String {
    let key = canonical_key(path);
    let key = key.trim_end_matches('/');
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key.to_owned()
    }
}

/// Whether `path` is explicitly named by one of the CLI `-e` paths: equal to
/// one, or inside one (a `-e` directory opts in the package it contains).
fn explicitly_selected(path: &Path, cli_paths: &[PathBuf]) -> bool {
    let target = comparable_key(path);
    cli_paths.iter().any(|cli| {
        let selected = comparable_key(cli);
        target == selected || (!selected.is_empty() && target.starts_with(&format!("{selected}/")))
    })
}

/// Drop default-disabled package paths from an effective extension plan
/// (see [`DEFAULT_DISABLED_PACKAGE_NAMES`]). Runs before the runtime starts,
/// so a skipped package never creates its QuickJS actor or evaluates a line
/// of JS.
///
/// Precedence, highest first:
/// 1. `enabled` (`defaultDisabledPackagesEnabled: true` in global settings)
///    keeps every path;
/// 2. an explicit CLI `-e/--extension` path naming the package or anything
///    inside it keeps that package's paths (explicit intent wins);
/// 3. otherwise the package's paths are dropped and one
///    [`DefaultDisabledPackageSkip`] is returned per package (not per path).
///
/// `paths` must already be the ordered effective plan (CLI paths first);
/// `cli_paths` is the raw `-e` list, used only for the opt-in decision.
pub fn filter_default_disabled_packages(
    paths: &[PathBuf],
    cli_paths: &[PathBuf],
    enabled: bool,
) -> (Vec<PathBuf>, Vec<DefaultDisabledPackageSkip>) {
    if enabled {
        return (paths.to_vec(), Vec::new());
    }
    // Packages explicitly named on the command line, by manifest identity.
    let cli_package_names: BTreeSet<&'static str> = cli_paths
        .iter()
        .filter_map(|path| default_disabled_package_name(path))
        .collect();
    let mut kept = Vec::with_capacity(paths.len());
    let mut skipped: Vec<DefaultDisabledPackageSkip> = Vec::new();
    for path in paths {
        let Some(name) = default_disabled_package_name(path) else {
            kept.push(path.clone());
            continue;
        };
        let opted_in = cli_package_names.contains(name) || explicitly_selected(path, cli_paths);
        if opted_in {
            kept.push(path.clone());
            continue;
        }
        if skipped.iter().any(|skip| skip.package_name == name) {
            // One diagnostic per package; the first dropped entry names the
            // path the user can pass to `-e`.
            continue;
        }
        skipped.push(DefaultDisabledPackageSkip {
            package_name: name.to_owned(),
            path: path.clone(),
            message: format!(
                "{name} is disabled by default: it starts a local web server and uses \
                 about 14 MB of QuickJS heap. Opt in with \
                 \"defaultDisabledPackagesEnabled\": true in the global settings file, \
                 or load it explicitly with -e \"{}\".",
                path.display()
            ),
        });
    }
    (kept, skipped)
}

/// Emit one routed `[ext]` diagnostic per skipped package. The messages are
/// stable (repeat-collapsed by the diagnostic router), so a host calls this
/// once when it builds its extension plan.
pub fn emit_default_disabled_package_diagnostics(skips: &[DefaultDisabledPackageSkip]) {
    for skip in skips {
        crate::ext_diag!("[ext] {}", skip.message);
    }
}

/// The default disabled-builtin names for the CLI (upstream `--no-mcp` adds
/// `"mcp"`). The pattern is defined so the flag semantics stay next to the
/// other loader filters.
pub fn disabled_from_flags(no_mcp: bool) -> Vec<String> {
    if no_mcp {
        vec!["mcp".to_owned()]
    } else {
        Vec::new()
    }
}

/// Synthetic `builtin:<name>` paths for every registered factory builtin
/// (default-enabled, like upstream builtin resources).
fn default_builtin_paths() -> Vec<PathBuf> {
    super::builtin::BUILTIN_EXTENSION_FACTORIES
        .iter()
        .filter(|def| def.default_enabled)
        .map(|def| PathBuf::from(format!("{BUILTIN_PATH_PREFIX}{}", def.name)))
        .collect()
}

/// The extension-plan policy selected by the CLI flags.
///
/// `--no-extensions` wins over `--only-builtin` when both are given: it is the
/// established "drop every default, keep only explicit `-e` paths" mode, while
/// `--only-builtin` is a strictly narrower filter layered on the normal
/// discovery plan. Defining the precedence here keeps the flag combination
/// out of the host code and testable without a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionPlanMode {
    /// Normal discovery: CLI paths, default builtins, settings/default paths,
    /// installed packages.
    Default,
    /// `--no-extensions`: only explicit `-e` paths (builtin or on-disk).
    NoExtensions,
    /// `--only-builtin`: explicit `-e builtin:<name>` paths plus the
    /// default-enabled builtin set; every on-disk path and package extension
    /// is ignored (reported separately).
    OnlyBuiltin,
}

/// Resolve the plan policy from the two flags; `--no-extensions` wins.
pub fn extension_plan_mode(no_extensions: bool, only_builtin: bool) -> ExtensionPlanMode {
    if no_extensions {
        ExtensionPlanMode::NoExtensions
    } else if only_builtin {
        ExtensionPlanMode::OnlyBuiltin
    } else {
        ExtensionPlanMode::Default
    }
}

/// Build the `--only-builtin` path plan: the explicit `-e builtin:<name>`
/// entries in CLI order (deduped exactly like [`order_extension_paths`]),
/// followed by every default-enabled builtin. Non-builtin `-e` paths never
/// enter the plan; they are returned in the second vector so the host can
/// emit the one-line "ignored on-disk extension path(s)" diagnostic.
///
/// Project/global settings paths, auto-discovery directories and installed
/// package extensions are not consulted at all, so this path pays no package
/// or directory scan, no SWC transpile and no QuickJS actor. An explicit
/// `-e builtin:<name>` (including a default-off builtin such as `phone`)
/// still loads its Rust factory; an explicit on-disk `-e <file>` does not.
pub fn only_builtin_paths(cli_paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut ordered, ignored) = filter_builtin_paths(cli_paths);
    let mut seen: BTreeSet<String> = ordered.iter().map(|path| canonical_key(path)).collect();
    for path in default_builtin_paths() {
        if seen.insert(canonical_key(&path)) {
            ordered.push(path);
        }
    }
    (ordered, ignored)
}

/// Filter `paths` down to its `builtin:<name>` entries, preserving order and
/// deduping by canonical key. On-disk paths are returned separately (ignored).
/// Unlike [`only_builtin_paths`], the default builtin set is NOT appended —
/// the SDK's [`crate::sdk::AgentBuilder::only_builtin`] filter shape.
pub fn filter_builtin_paths(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut ignored: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        if is_builtin_path(&path.to_string_lossy()) {
            if seen.insert(canonical_key(path)) {
                kept.push(path.clone());
            }
        } else {
            ignored.push(path.clone());
        }
    }
    (kept, ignored)
}

/// Assemble the effective extension path list (upstream ordering):
/// CLI `-e` paths first, then settings/default paths (project-local before
/// user-global), then the default-enabled factory builtins, deduped by
/// canonical path. When `no_extensions` is set only the CLI paths survive —
/// an explicit `-e` still loads.
///
/// Builtins come LAST (upstream `package-manager.ts:986-1001` adds them after
/// every file resource; `resource-loader.ts:608-617` merges the CLI list in
/// front of the discovered one): first-wins lookups (tool/command names,
/// renderer chains, event order) therefore prefer file extensions over
/// builtins, matching the reference. A `builtin:<name>` that also appears on
/// the CLI keeps its CLI position.
pub fn order_extension_paths(
    cli_paths: &[PathBuf],
    default_paths: &[PathBuf],
    no_extensions: bool,
) -> Vec<PathBuf> {
    let mut ordered: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let push = |path: PathBuf, seen: &mut BTreeSet<String>, ordered: &mut Vec<PathBuf>| {
        let canonical = canonical_key(&path);
        if seen.insert(canonical) {
            ordered.push(path);
        }
    };
    if no_extensions {
        for path in cli_paths {
            push(path.clone(), &mut seen, &mut ordered);
        }
        return ordered;
    }
    for path in cli_paths {
        push(path.clone(), &mut seen, &mut ordered);
    }
    for path in default_paths {
        push(path.clone(), &mut seen, &mut ordered);
    }
    // Builtins last: a file extension wins every first-wins conflict
    // (upstream package-manager.ts:986-1001).
    for path in default_builtin_paths() {
        push(path, &mut seen, &mut ordered);
    }
    ordered
}

/// Canonical dedup key: absolute path, forward slashes, lowercased on
/// Windows (case-insensitive filesystem). Synthetic paths (`builtin:*`,
/// `<inline:*>`) key as written.
fn canonical_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if is_builtin_path(&text) || text.starts_with('<') {
        return text;
    }
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved.to_string_lossy().replace('\\', "/"),
        Err(_) => {
            let text = text.trim_end_matches('/');
            if cfg!(windows) {
                text.to_lowercase()
            } else {
                text.to_owned()
            }
        }
    }
}

/// Errors for `builtin:<name>` paths that name no registered builtin
/// (upstream: `result.errors.push({ path, error: "Unknown built-in
/// extension: " + path })`).
pub fn unknown_builtin_errors(
    builtin_names: &[String],
    diagnostics: &mut Vec<super::registry::PluginDiagnostic>,
) {
    for name in builtin_names {
        if super::builtin::builtin_extension_def(name).is_none() {
            diagnostics.push(super::registry::PluginDiagnostic {
                kind: super::registry::DiagnosticKind::Error,
                path: format!("{BUILTIN_PATH_PREFIX}{name}"),
                message: super::builtin::missing_builtin_message(name),
            });
        }
    }
}

/// Test seam: the native-precedence rules (integration tests live in
/// `tests/`; inline test modules are forbidden in `src/`).
#[doc(hidden)]
pub fn native_precedence_for_tests(index: usize, prefer_native: bool) -> bool {
    native_precedence_applies(index, prefer_native)
}

/// Test seam: number of native/JS package pairs in the precedence table.
#[doc(hidden)]
pub fn native_precedence_len_for_tests() -> usize {
    NATIVE_PRECEDENCE.len()
}

/// Test seam: package-path matching used by the precedence rule.
#[doc(hidden)]
pub fn is_js_package_path_for_tests(path: &Path, package: &str) -> bool {
    is_js_package_path(path, package)
}

