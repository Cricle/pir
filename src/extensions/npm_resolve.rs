//! npm-style bare-specifier resolution: the Node `node_modules` walk plus a
//! practical subset of the `"exports"` field (condition objects, arrays,
//! `*` subpath patterns), used by the module loader (ESM import) and by the
//! `npm` host ops behind `createRequire` (`require`). Pure functions over the
//! filesystem: no network, and only the bounded, fingerprint-validated
//! package-manifest cache below.
//!
//! Declared from `loader.rs` via `#[path]` so the resolver sits next to its
//! only integration point without touching `extensions/mod.rs`.

use std::borrow::Cow;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Bounded package-manifest discovery cache
// ---------------------------------------------------------------------------

/// Fingerprint of one `package.json` read: length plus modification time.
/// A manifest is only reused while BOTH match, so an edited/replaced manifest
/// is re-read; a touched-but-identical manifest costs one stat.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestFingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// Bounded, fingerprint-keyed cache of parsed `package.json` values used by
/// the host's package discovery (bare-specifier resolution, exports/main
/// probing, module-kind detection).
///
/// The discovery walk re-reads the same handful of manifests many times per
/// extension load (and across `/reload` / repeated sessions in one process).
/// Every hit is validated against the live file's length+mtime, so a package
/// or config change invalidates its entry instead of serving a stale
/// manifest; the cache never crosses a fingerprint mismatch. The entry count
/// is capped ([`Self::DEFAULT_CAPACITY`], oldest evicted first) so a large
/// `node_modules` walk cannot grow it unboundedly.
#[derive(Debug)]
pub struct ManifestCache {
    capacity: usize,
    entries: Vec<(PathBuf, ManifestFingerprint, Arc<serde_json::Value>)>,
}

impl Default for ManifestCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

impl ManifestCache {
    /// Default bound: enough for a large project's package manifests, small
    /// enough that the retained parsed JSON stays cheap.
    pub const DEFAULT_CAPACITY: usize = 128;

    /// A cache holding at most `capacity` manifests (clamped to `>= 1`).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Vec::new(),
        }
    }

    /// Number of cached manifests (test/diagnostic seam).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured bound (test seam).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The cached parsed manifest at `path`, re-reading (and re-fingerprinting)
    /// only when the file changed, is new, or was evicted. The returned `Arc`
    /// is shared with the cache entry, so repeated hits copy nothing.
    pub fn get_or_load(
        &mut self,
        path: &Path,
    ) -> Result<Arc<serde_json::Value>, ManifestCacheError> {
        let metadata =
            std::fs::metadata(path).map_err(|error| ManifestCacheError(error.to_string()))?;
        let fingerprint = ManifestFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
        if let Some(index) = self.entries.iter().position(|(cached, ..)| cached == path) {
            if self.entries[index].1 == fingerprint {
                // LRU refresh: move the hit to the back (eviction order).
                let entry = self.entries.remove(index);
                self.entries.push(entry);
                let last = self.entries.len() - 1;
                return Ok(Arc::clone(&self.entries[last].2));
            }
            self.entries.remove(index);
        }
        let text =
            std::fs::read_to_string(path).map_err(|error| ManifestCacheError(error.to_string()))?;
        let value = Arc::new(
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|error| ManifestCacheError(error.to_string()))?,
        );
        if self.entries.len() >= self.capacity {
            self.entries.remove(0);
        }
        self.entries
            .push((path.to_path_buf(), fingerprint, Arc::clone(&value)));
        Ok(value)
    }

    /// Drop every entry (used by tests and by an explicit host reset).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// One manifest read/parse failure (the caller maps it to its own error type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestCacheError(pub String);

impl fmt::Display for ManifestCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ManifestCacheError {}

fn global_manifest_cache() -> &'static Mutex<ManifestCache> {
    static CACHE: OnceLock<Mutex<ManifestCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ManifestCache::default()))
}

/// Read one manifest through the process-wide bounded cache. Best-effort
/// sites use `.ok()`; resolution sites map the error into
/// [`ResolveError::Manifest`].
pub(crate) fn read_manifest_cached(
    manifest: &Path,
) -> Result<Arc<serde_json::Value>, ManifestCacheError> {
    global_manifest_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_load(manifest)
}

/// Drop the process-wide package-manifest cache (a host that reloads
/// settings/packages in place calls this; fingerprint validation already
/// makes it safe to keep, this just releases the memory).
pub fn clear_manifest_cache() {
    global_manifest_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Identifier characters for the lexical module-syntax / regex scans
/// (`has_module_syntax`, `skip_regex_literal`).
const IDENT_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_$";

/// Which condition set an `"exports"` lookup uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// ESM `import`: `["node", "import", "default"]`.
    Esm,
    /// CommonJS `require`: `["node", "require", "default"]`.
    Require,
}

impl ResolveMode {
    /// Condition keys tried, in order (object keys are matched in the order
    /// the package declared them and filtered against this set).
    pub fn conditions(self) -> &'static [&'static str] {
        match self {
            ResolveMode::Esm => &["node", "import", "default"],
            ResolveMode::Require => &["node", "require", "default"],
        }
    }
}

/// Runtime treatment of a resolved file, from its name and package scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Esm,
    Cjs,
    Json,
    Native,
}

/// One successful bare-specifier resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The absolute file the specifier resolved to.
    pub path: PathBuf,
    /// The package directory (`…/node_modules/<pkg>`), for allowed-roots.
    pub package_root: PathBuf,
    pub package_name: String,
}

/// A resolution failure with the Node-shaped code the JS layer reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No `node_modules/<pkg>` exists on the walk-up from the importer.
    NotFound { package: String, importer: PathBuf },
    /// `"exports"` exists but no key/condition matched the subpath.
    NotExported {
        package: String,
        subpath: String,
        manifest: PathBuf,
    },
    /// An `"exports"` target that does not start with `./` or escapes the
    /// package root.
    InvalidTarget { target: String, manifest: PathBuf },
    /// `package.json` unreadable or not JSON.
    Manifest { manifest: PathBuf, message: String },
    /// The resolution landed on a native addon (`.node`).
    Native { path: PathBuf },
    /// A relative/absolute path specifier that exists nowhere.
    ModuleNotFound {
        specifier: String,
        importer: PathBuf,
    },
    /// A `#`-fragment specifier with no matching entry in the nearest
    /// package's `"imports"` field (or no `"imports"`/no package at all).
    PackageImportNotDefined {
        specifier: String,
        importer: PathBuf,
    },
}

impl ResolveError {
    /// Node error code for the JS `Error.code` field.
    pub fn code(&self) -> &'static str {
        match self {
            ResolveError::NotFound { .. } | ResolveError::ModuleNotFound { .. } => {
                "ERR_MODULE_NOT_FOUND"
            }
            ResolveError::PackageImportNotDefined { .. } => "ERR_PACKAGE_IMPORT_NOT_DEFINED",
            ResolveError::NotExported { .. } => "ERR_PACKAGE_PATH_NOT_EXPORTED",
            ResolveError::InvalidTarget { .. } => "ERR_INVALID_PACKAGE_TARGET",
            ResolveError::Manifest { .. } => "ERR_INVALID_PACKAGE_CONFIG",
            ResolveError::Native { .. } => "ERR_DLOPEN_DISABLED",
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NotFound { package, importer } => write!(
                f,
                "cannot find package '{package}' (imported from {})",
                importer.display()
            ),
            ResolveError::NotExported {
                package,
                subpath,
                manifest,
            } => write!(
                f,
                "package '{package}' has no matching export '{subpath}' in {}",
                manifest.display()
            ),
            ResolveError::InvalidTarget { target, manifest } => write!(
                f,
                "ERR_INVALID_PACKAGE_TARGET: invalid target '{target}' in {}",
                manifest.display()
            ),
            ResolveError::Manifest { manifest, message } => write!(
                f,
                "invalid package.json at {}: {message}",
                manifest.display()
            ),
            ResolveError::Native { path } => write!(
                f,
                "native addons (.node) are not supported in this runtime ({})",
                path.display()
            ),
            ResolveError::ModuleNotFound {
                specifier,
                importer,
            } => write!(
                f,
                "cannot find module '{specifier}' (imported from {})",
                importer.display()
            ),
            ResolveError::PackageImportNotDefined { specifier, importer } => write!(
                f,
                "package import specifier '{specifier}' is not defined in the nearest package.json (imported from {})",
                importer.display()
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Split a bare specifier into `(package_name, subpath)` where subpath is
/// `"."` for the package root and `"./a/b"` otherwise. The package name is
/// always a slice of `specifier` (`@scope/name` is contiguous there too) and
/// the root subpath borrows a static `"."`, so the common `pkg` /
/// `@scope/pkg` forms allocate nothing; only a real subpath is built.
/// Returns `None` for a malformed scoped specifier (`@scope` without a name).
fn split_specifier(specifier: &str) -> Option<(&str, Cow<'_, str>)> {
    let mut segments = specifier.split('/');
    let first = segments.next()?;
    if first.is_empty() {
        return None;
    }
    let name = if first.starts_with('@') {
        let second = segments.next().filter(|part| !part.is_empty())?;
        &specifier[..first.len() + 1 + second.len()]
    } else {
        first
    };
    let mut subpath: Option<String> = None;
    for part in segments {
        let text = subpath.get_or_insert_with(|| String::from("."));
        push_subpath_segment(text, part);
    }
    Some((name, subpath.map_or(Cow::Borrowed("."), Cow::Owned)))
}

fn push_subpath_segment(subpath: &mut String, part: &str) {
    if subpath == "." {
        subpath.clear();
        subpath.push_str("./");
        subpath.push_str(part);
    } else {
        subpath.push('/');
        subpath.push_str(part);
    }
}

/// Resolve a bare specifier (`pkg`, `@scope/pkg`, `pkg/sub`) from the
/// importing module's directory: walk `node_modules` upward, then apply the
/// package's `"exports"` (when present) or the legacy main/index rules.
pub fn resolve_bare(
    importer_dir: &Path,
    specifier: &str,
    mode: ResolveMode,
) -> Result<Resolved, ResolveError> {
    resolve_bare_searching(importer_dir, &[], specifier, mode)
}

/// [`resolve_bare`] with Node-`NODE_PATH`-style fallback roots: when the
/// `node_modules` walk-up from the importer finds nothing, each search root
/// is tried as a `node_modules` parent in order (project-local wins — the
/// walk-up always runs first). The script runner (`pir js`) contributes the
/// user npm root (`~/.pi/agent/npm`), because a standalone script has no
/// extension directory to derive it from.
pub fn resolve_bare_searching(
    importer_dir: &Path,
    search_roots: &[PathBuf],
    specifier: &str,
    mode: ResolveMode,
) -> Result<Resolved, ResolveError> {
    let (package_name, subpath) =
        split_specifier(specifier).ok_or_else(|| ResolveError::NotFound {
            package: specifier.to_string(),
            importer: importer_dir.to_path_buf(),
        })?;
    // `#`-fragment specifiers are package imports, not package names: resolve
    // them against the nearest enclosing package's `"imports"` field.
    if package_name.starts_with('#') {
        return resolve_imports_specifier(importer_dir, specifier, mode);
    }
    // The importer walk-up first, then one walk-up per search root, in order.
    let starts = std::iter::once(importer_dir).chain(search_roots.iter().map(PathBuf::as_path));
    for start in starts {
        let mut cursor = Some(start);
        while let Some(dir) = cursor {
            let package_root = dir.join("node_modules").join(package_name);
            if package_root.is_dir() {
                return resolve_in_package(
                    &package_root,
                    package_name,
                    subpath.as_ref(),
                    specifier,
                    importer_dir,
                    mode,
                );
            }
            cursor = dir.parent();
        }
    }
    Err(ResolveError::NotFound {
        package: package_name.to_string(),
        importer: importer_dir.to_path_buf(),
    })
}

/// Resolve a relative (`./x`) or absolute path specifier with Node's legacy
/// file/directory probing (`LOAD_AS_FILE` + `LOAD_AS_DIRECTORY`). Used by
/// `require`; ESM relative imports go through the loader's own candidate
/// list (which also tries TypeScript renames).
pub fn resolve_path_specifier(
    importer_dir: &Path,
    specifier: &str,
) -> Result<PathBuf, ResolveError> {
    let joined = if specifier.starts_with('.') {
        importer_dir.join(specifier)
    } else {
        PathBuf::from(specifier)
    };
    let normalized = lexical_normalize(&joined);
    if let Some(path) = probe_legacy(&normalized) {
        if is_native_addon(&path) {
            return Err(ResolveError::Native { path });
        }
        return Ok(path);
    }
    Err(ResolveError::ModuleNotFound {
        specifier: specifier.to_string(),
        importer: importer_dir.to_path_buf(),
    })
}

fn resolve_in_package(
    package_root: &Path,
    package_name: &str,
    subpath: &str,
    specifier: &str,
    importer_dir: &Path,
    mode: ResolveMode,
) -> Result<Resolved, ResolveError> {
    let manifest = package_root.join("package.json");
    let mut main: Option<String> = None;
    if manifest.is_file() {
        // Fingerprint-validated discovery cache: repeated loads/resolutions
        // re-read a changed manifest, never a stale one.
        let json = read_manifest_cached(&manifest).map_err(|error| ResolveError::Manifest {
            manifest: manifest.clone(),
            message: error.to_string(),
        })?;
        if let Some(exports) = json.get("exports") {
            let target =
                resolve_exports(exports, subpath, mode.conditions(), package_name, &manifest)?;
            let path =
                materialize_target(package_root, &target, &manifest, specifier, importer_dir)?;
            return Ok(finish_resolved(path, package_root, package_name));
        }
        main = json
            .get("main")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
    }
    let path = if subpath == "." {
        main.and_then(|main| probe_legacy(&lexical_normalize(&package_root.join(main))))
            .or_else(|| {
                ["index.js", "index.json", "index.node"]
                    .iter()
                    .map(|name| package_root.join(name))
                    .find(|candidate| candidate.is_file())
            })
            .ok_or_else(|| ResolveError::ModuleNotFound {
                specifier: specifier.to_string(),
                importer: importer_dir.to_path_buf(),
            })?
    } else {
        let relative = subpath.trim_start_matches("./");
        probe_legacy(&lexical_normalize(&package_root.join(relative))).ok_or_else(|| {
            ResolveError::ModuleNotFound {
                specifier: specifier.to_string(),
                importer: importer_dir.to_path_buf(),
            }
        })?
    };
    if is_native_addon(&path) {
        return Err(ResolveError::Native { path });
    }
    Ok(finish_resolved(path, package_root, package_name))
}

fn finish_resolved(path: PathBuf, package_root: &Path, package_name: &str) -> Resolved {
    Resolved {
        path,
        package_root: package_root.to_path_buf(),
        package_name: package_name.to_string(),
    }
}

/// Resolve a `#`-fragment specifier (`#foo`, `#foo/*`) against the nearest
/// enclosing package's `"imports"` field (Node package-imports semantics):
/// the first `package.json` up the tree owns the `#` namespace; exact keys
/// match first, then `*` pattern keys use the longest prefix; values resolve
/// like `"exports"` targets against the caller's condition set.
fn resolve_imports_specifier(
    importer_dir: &Path,
    specifier: &str,
    mode: ResolveMode,
) -> Result<Resolved, ResolveError> {
    let not_defined = || ResolveError::PackageImportNotDefined {
        specifier: specifier.to_string(),
        importer: importer_dir.to_path_buf(),
    };
    let mut cursor = Some(importer_dir);
    while let Some(dir) = cursor {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            // The nearest manifest is the package boundary — even without an
            // `"imports"` field, the specifier is undefined past it.
            let json = read_manifest_cached(&manifest).map_err(|error| ResolveError::Manifest {
                manifest: manifest.clone(),
                message: error.to_string(),
            })?;
            let Some(imports) = json.get("imports") else {
                return Err(not_defined());
            };
            let package_name = json
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            return resolve_imports_in_package(
                dir,
                imports,
                specifier,
                mode,
                &manifest,
                importer_dir,
                &package_name,
            );
        }
        cursor = dir.parent();
    }
    Err(not_defined())
}

fn resolve_imports_in_package(
    package_root: &Path,
    imports: &serde_json::Value,
    specifier: &str,
    mode: ResolveMode,
    manifest: &Path,
    importer_dir: &Path,
    package_name: &str,
) -> Result<Resolved, ResolveError> {
    let not_defined = || ResolveError::PackageImportNotDefined {
        specifier: specifier.to_string(),
        importer: importer_dir.to_path_buf(),
    };
    let map = match imports {
        serde_json::Value::Object(map) => map,
        _ => return Err(not_defined()),
    };
    let target = if let Some(value) = map.get(specifier) {
        resolve_target_value(value, mode.conditions()).ok_or_else(not_defined)?
    } else {
        // Pattern keys (`#styles/*`), longest matching prefix wins — the
        // same rule `"exports"` patterns use.
        let mut best: Option<(usize, String)> = None;
        for (key, value) in map {
            let Some((prefix, suffix)) = key.split_once('*') else {
                continue;
            };
            if specifier.len() < prefix.len() + suffix.len()
                || !specifier.starts_with(prefix)
                || !specifier.ends_with(suffix)
            {
                continue;
            }
            if best.as_ref().is_some_and(|(len, _)| *len >= prefix.len()) {
                continue;
            }
            if let Some(matched) = resolve_target_value(value, mode.conditions()) {
                let wildcard = &specifier[prefix.len()..specifier.len() - suffix.len()];
                best = Some((prefix.len(), matched.replace('*', wildcard)));
            }
        }
        best.map(|(_, target)| target).ok_or_else(not_defined)?
    };
    // Import targets are package-internal: they must start with `./` (bare or
    // `#` targets would need recursive resolution — not supported).
    let invalid = || ResolveError::InvalidTarget {
        target: target.clone(),
        manifest: manifest.to_path_buf(),
    };
    let Some(relative) = target.strip_prefix("./") else {
        return Err(invalid());
    };
    let joined = lexical_normalize(&package_root.join(relative));
    if !path_within(&joined, &lexical_normalize(package_root)) {
        return Err(invalid());
    }
    let path = probe_export(&joined).ok_or_else(|| ResolveError::ModuleNotFound {
        specifier: specifier.to_string(),
        importer: importer_dir.to_path_buf(),
    })?;
    if is_native_addon(&path) {
        return Err(ResolveError::Native { path });
    }
    Ok(finish_resolved(path, package_root, package_name))
}

/// Pick the `"exports"` target for `subpath` (canonical `"."` / `"./x"`).
/// Object keys are scanned in declaration order (first matching condition
/// wins), `"types"` is always skipped, arrays are tried in order, and
/// `*` pattern keys use the longest matching prefix.
fn resolve_exports(
    exports: &serde_json::Value,
    subpath: &str,
    conditions: &[&str],
    package_name: &str,
    manifest: &Path,
) -> Result<String, ResolveError> {
    let not_exported = || ResolveError::NotExported {
        package: package_name.to_string(),
        subpath: subpath.to_string(),
        manifest: manifest.to_path_buf(),
    };
    let subpath_map = match exports {
        serde_json::Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => Some(map),
        _ => None,
    };
    let Some(map) = subpath_map else {
        // Sugar form: the whole value describes ".".
        if subpath != "." {
            return Err(not_exported());
        }
        return resolve_target_value(exports, conditions).ok_or_else(not_exported);
    };
    if let Some(value) = map.get(subpath) {
        // Exact key: Node does not fall back to patterns after an exact hit.
        return resolve_target_value(value, conditions).ok_or_else(not_exported);
    }
    let mut best: Option<(usize, String)> = None;
    for (key, value) in map {
        let Some((prefix, suffix)) = key.split_once('*') else {
            continue;
        };
        if subpath.len() < prefix.len() + suffix.len()
            || !subpath.starts_with(prefix)
            || !subpath.ends_with(suffix)
        {
            continue;
        }
        if best.as_ref().is_some_and(|(len, _)| *len >= prefix.len()) {
            continue;
        }
        if let Some(target) = resolve_target_value(value, conditions) {
            let matched = &subpath[prefix.len()..subpath.len() - suffix.len()];
            best = Some((prefix.len(), target.replace('*', matched)));
        }
    }
    best.map(|(_, target)| target).ok_or_else(not_exported)
}

/// Resolve one `"exports"` value against the condition set: strings pass
/// through, arrays are tried in order, nested condition objects match the
/// highest-priority recognized condition. `"types"` never matches (it is not
/// in the condition set). `None` = no condition matched.
///
/// Note: serde_json maps are key-sorted, so declaration order is not
/// observable; matching by condition priority (`node` → `import`/`require` →
/// `default`) reproduces the practical intent of Node's first-match rule.
fn resolve_target_value(value: &serde_json::Value, conditions: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::String(target) => Some(target.clone()),
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|item| resolve_target_value(item, conditions)),
        serde_json::Value::Object(map) => conditions.iter().find_map(|condition| {
            let sub = map.get(*condition)?;
            resolve_target_value(sub, conditions)
        }),
        _ => None,
    }
}

/// Turn an `"exports"` target into an existing absolute file: must start
/// with `./`, must stay inside the package root, probing extensions/index
/// files when the target is not an exact file. A landing on `.node` is
/// rejected here (the loader never gets a chance to see it).
fn materialize_target(
    package_root: &Path,
    target: &str,
    manifest: &Path,
    specifier: &str,
    importer_dir: &Path,
) -> Result<PathBuf, ResolveError> {
    let invalid = || ResolveError::InvalidTarget {
        target: target.to_string(),
        manifest: manifest.to_path_buf(),
    };
    let Some(relative) = target.strip_prefix("./") else {
        return Err(invalid());
    };
    let joined = lexical_normalize(&package_root.join(relative));
    if !path_within(&joined, &lexical_normalize(package_root)) {
        return Err(invalid());
    }
    let path = probe_export(&joined).ok_or_else(|| ResolveError::ModuleNotFound {
        specifier: specifier.to_string(),
        importer: importer_dir.to_path_buf(),
    })?;
    if is_native_addon(&path) {
        return Err(ResolveError::Native { path });
    }
    Ok(path)
}

/// File probing for `"exports"`/`"imports"` targets: exact file, then
/// extension completion (`.js`/`.mjs`/`.cjs`/`.json`/`.node` plus the
/// TypeScript source extensions `.ts`/`.mts`/`.cts`), then `index.*` for a
/// directory (a forgiving superset of Node's exact-match ESM rule).
///
/// The TS candidates matter for publish-source packages: an extension shipping
/// `src/*.ts` with `"imports": { "#src/*": "./src/*" }` resolves `#src/foo` to
/// nothing under a JS-only probe (`@gotgenes/pi-permission-system` loads
/// through such specifiers), even though NodeNext-style `.ts` source is exactly
/// what the transpiling loader wants.
fn probe_export(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    if base.extension().is_none() {
        for ext in ["js", "mjs", "cjs", "json", "node", "ts", "mts", "cts"] {
            let candidate = base.with_extension(ext);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if base.is_dir() {
        for index in [
            "index.js",
            "index.mjs",
            "index.cjs",
            "index.json",
            "index.ts",
            "index.mts",
            "index.cts",
        ] {
            let candidate = base.join(index);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Legacy `LOAD_AS_FILE` + `LOAD_AS_DIRECTORY` probing: exact file,
/// extension completion (`.js`/`.json`/`.node`), then a directory's
/// `package.json` main or `index.*`.
fn probe_legacy(base: &Path) -> Option<PathBuf> {
    if let Some(path) = probe_file_legacy(base) {
        return Some(path);
    }
    if base.is_dir() {
        if let Ok(json) = read_manifest_cached(&base.join("package.json")) {
            if let Some(main) = json.get("main").and_then(serde_json::Value::as_str) {
                if let Some(path) = probe_file_legacy(&lexical_normalize(&base.join(main))) {
                    return Some(path);
                }
            }
        }
        for index in ["index.js", "index.json", "index.node"] {
            let candidate = base.join(index);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn probe_file_legacy(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    // LOAD_AS_FILE extension probing is unconditional in Node and APPENDS the
    // extension (require("./util.inspect") finds "util.inspect.js"); a named
    // extension on the base is data, not a stop signal.
    for ext in ["js", "json", "node"] {
        let mut name = base.as_os_str().to_os_string();
        name.push(".");
        name.push(ext);
        let candidate = PathBuf::from(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Whether a resolved path is a native addon, without classifying the module:
/// [`module_kind`] reports `Native` exactly for a `.node` extension (every
/// other extension, and no extension at all, yields one of the other kinds),
/// so the `.js` classification walk — nearest `package.json`, then a
/// module-syntax scan of the whole source — is never needed for this check.
fn is_native_addon(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("node")
}

/// Classify a file by extension; a `.js` file takes its kind from the
/// nearest enclosing `package.json` (`"type": "module"` → ESM). Node 22+
/// module-syntax detection applies when the nearest manifest (if any) has no
/// `"type"` field: the source is scanned for import/export module syntax and
/// classified ESM when it has some, CommonJS otherwise. Any other extension
/// (`.ts`, `.mts`, extensionless, …) is treated as ESM for the transpile
/// pipeline.
pub fn module_kind(path: &Path) -> ModuleKind {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("mjs") => ModuleKind::Esm,
        Some("cjs") => ModuleKind::Cjs,
        // TypeScript's NodeNext spellings: `.mts` is ESM, `.cts` is CommonJS
        // (it compiles to `.mjs` / `.cjs` respectively).
        Some("mts") => ModuleKind::Esm,
        Some("cts") => ModuleKind::Cjs,
        Some("json") => ModuleKind::Json,
        Some("node") => ModuleKind::Native,
        Some("js") => {
            let mut cursor = path.parent();
            while let Some(dir) = cursor {
                let manifest = dir.join("package.json");
                if manifest.is_file() {
                    let kind = read_manifest_cached(&manifest).ok().and_then(|json| {
                        json.get("type")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    });
                    return match kind.as_deref() {
                        // Nearest manifest decides; an explicit type ends the
                        // ambiguity (Node semantics).
                        Some("module") => ModuleKind::Esm,
                        Some(_) => ModuleKind::Cjs,
                        // Manifest present but no `type`: ambiguous — fall
                        // through to syntax detection.
                        None => break,
                    };
                }
                cursor = dir.parent();
            }
            // Ambiguous `.js` (no `type` anywhere up the tree): Node 22+
            // detects module syntax. An unreadable source keeps the old Cjs
            // default.
            std::fs::read_to_string(path)
                .map(|source| {
                    if has_module_syntax(&source) {
                        ModuleKind::Esm
                    } else {
                        ModuleKind::Cjs
                    }
                })
                .unwrap_or(ModuleKind::Cjs)
        }
        _ => ModuleKind::Esm,
    }
}

/// Lexical scan for ES module syntax that is invalid in a CommonJS script:
/// static `import` declarations (including `import.meta`), and `export`
/// declarations. Strings, template literals and comments are skipped;
/// dynamic `import(...)` and member accesses (`.import`) are not module
/// syntax. This mirrors Node's unflagged `--experimental-detect-module`
/// (the default since Node 22) at the token level.
pub fn has_module_syntax(source: &str) -> bool {
    // Byte length bounds the char count (every char is at least one byte), so
    // reserving it up front avoids the reallocations `collect()` performs while
    // growing from the iterator's `len / 4` lower bound. This runs on every
    // ambiguous `.js` of a manifest without a `"type"` field.
    let mut chars: Vec<char> = Vec::with_capacity(source.len());
    chars.extend(source.chars());
    #[derive(PartialEq)]
    enum Mode {
        Code,
        Str,
        Template,
        LineComment,
        BlockComment,
    }
    let mut mode = Mode::Code;
    let mut template_stack: Vec<()> = Vec::new();
    let mut quote = '"';
    let mut last_word = String::new();
    // The last non-identifier code char before the current word (for member
    // access exclusion: `a.import` / `obj.export` are properties).
    let mut prev_char: Option<char> = None;
    // The last non-whitespace char seen anywhere (for the regex-vs-division
    // decision at a `/`: `total / 2` is division, `x = /re/` is a literal).
    let mut last_nonws: Option<char> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match mode {
            Mode::Str => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == quote {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::Template => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '`' {
                    mode = Mode::Code;
                    i += 1;
                    continue;
                }
                if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                    template_stack.push(());
                    mode = Mode::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Mode::LineComment => {
                if c == '\n' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::BlockComment => {
                if c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    mode = Mode::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Mode::Code => {
                if c == '"' || c == '\'' {
                    mode = Mode::Str;
                    quote = c;
                    last_word.clear();
                    prev_char = Some(c);
                    i += 1;
                    continue;
                }
                if c == '`' {
                    mode = Mode::Template;
                    last_word.clear();
                    prev_char = Some(c);
                    i += 1;
                    continue;
                }
                if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    mode = Mode::LineComment;
                    last_word.clear();
                    i += 2;
                    continue;
                }
                if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                    mode = Mode::BlockComment;
                    last_word.clear();
                    i += 2;
                    continue;
                }
                if c == '}' && !template_stack.is_empty() {
                    template_stack.pop();
                    mode = Mode::Template;
                    last_word.clear();
                    prev_char = Some(c);
                    i += 1;
                    continue;
                }
                if IDENT_CHARS.contains(c) {
                    last_word.push(c);
                    last_nonws = Some(c);
                    i += 1;
                    continue;
                }
                // Regex literal: skip its body so `/export/` or `/import x/`
                // inside a pattern is not mistaken for module syntax (Node
                // decides through a real parse; this is the lexical stand-in,
                // mirroring the lexer's expression-position rule).
                if c == '/' && regex_allowed(last_nonws, &last_word) {
                    if let Some(end) = skip_regex_literal(&chars, i) {
                        i = end;
                        last_word.clear();
                        last_nonws = Some('/');
                        continue;
                    }
                }
                // Word boundary: decide on the completed keyword.
                if !last_word.is_empty() {
                    let member_access = prev_char == Some('.');
                    let next = chars[i..].iter().find(|ch| !ch.is_whitespace());
                    if last_word == "import" && !member_access {
                        match next {
                            // Dynamic import is valid CommonJS.
                            Some('(') => {}
                            // Object keys (`{ import: x }`) and TS-only
                            // `import x = require(...)` are not ESM syntax.
                            Some(':') | Some('=') => {}
                            // `import.meta` is ESM-only.
                            Some('.') => return true,
                            // Static import declaration.
                            _ => return true,
                        }
                    } else if last_word == "export" && !member_access {
                        match next {
                            // Object key (`{ export: fn }`) is not ESM syntax.
                            Some(':') => {}
                            _ => return true,
                        }
                    }
                    last_word.clear();
                }
                if !c.is_whitespace() {
                    prev_char = Some(c);
                    last_nonws = Some(c);
                }
                i += 1;
            }
        }
    }
    false
}

/// Is the `/` at this position a regex-literal opener rather than division?
/// Classic lexer heuristic: a regex may follow a keyword like `return`/
/// `typeof`/`case`, an operator/punctuator, or nothing (start of file); after
/// an identifier, `)`, `]`, `.` or a quoted string it is division.
fn regex_allowed(last_nonws: Option<char>, last_word: &str) -> bool {
    match last_word {
        "return" | "typeof" | "instanceof" | "in" | "of" | "new" | "delete" | "void" | "do"
        | "else" | "yield" | "await" | "case" | "throw" => return true,
        _ => {}
    }
    match last_nonws {
        None => true,
        Some(c) => matches!(
            c,
            '(' | ','
                | '='
                | ':'
                | '['
                | '!'
                | '&'
                | '|'
                | '?'
                | '{'
                | '}'
                | ';'
                | '+'
                | '-'
                | '*'
                | '%'
                | '^'
                | '~'
                | '<'
                | '>'
        ),
    }
}

/// Skip a regex literal starting at `chars[start] == '/'`; returns the index
/// just past the closing `/` and its flags. `None` when no terminator is
/// found on the same logical line — the `/` was division after all, and the
/// caller re-processes it as one. `[...]` classes may contain `/` unescaped.
fn skip_regex_literal(chars: &[char], start: usize) -> Option<usize> {
    let mut in_class = false;
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => i += 2,
            '\n' => return None,
            '[' => {
                in_class = true;
                i += 1;
            }
            ']' => {
                in_class = false;
                i += 1;
            }
            '/' if !in_class => {
                i += 1;
                while i < chars.len() && IDENT_CHARS.contains(chars[i]) {
                    i += 1;
                }
                return Some(i);
            }
            _ => i += 1,
        }
    }
    None
}

/// Resolve `.` / `..` components lexically (no filesystem access).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Component-wise prefix test (case-insensitive on Windows), for the
/// package-root escape check on `"exports"` targets.
fn path_within(candidate: &Path, root: &Path) -> bool {
    let key = |component: &Component<'_>| {
        let text = component.as_os_str().to_string_lossy().into_owned();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    let root_parts: Vec<String> = root.components().map(|c| key(&c)).collect();
    if root_parts.is_empty() {
        return false;
    }
    let candidate_parts: Vec<String> = candidate.components().map(|c| key(&c)).collect();
    candidate_parts.len() >= root_parts.len()
        && root_parts.iter().zip(&candidate_parts).all(|(a, b)| a == b)
}
