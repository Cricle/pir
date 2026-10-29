//! Extension discovery and QuickJS module loading.
//!
//! Discovery mirrors upstream `pi` (`loader.ts:692-766`): for each extension
//! directory, a `package.json` with a `"pi": { "extensions": [...] }`
//! manifest wins, then `index.ts`, then `index.js`; otherwise top-level
//! `*.ts`/`*.js` files load directly (single level, no recursion).
//!
//! Module loading: extension sources are transpiled TypeScript (or plain JS)
//! registered under synthetic `pir-ext:` module names; bare-specifier imports
//! of the upstream pi packages and `typebox` resolve to host-provided virtual
//! modules (see [`crate::extensions::api`]). Relative imports resolve against
//! the importing extension file's directory.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use rquickjs::loader::ImportAttributes;
use rquickjs::{Ctx, Module, Result as JsResult};

use super::transpile::transpile;

// Declared here via #[path] (not in extensions/mod.rs) so the resolver file
// lives beside the loader it integrates with.
#[path = "npm_resolve.rs"]
pub mod npm_resolve;

// Same #[path] pattern: the CJS named-export lexer feeding the interop
// wrapper below.
#[path = "cjs_lexer.rs"]
pub mod cjs_lexer;

pub const EXT_MODULE_PREFIX: &str = "pir-ext:";

// Engine-free data types shared with the host: defined in
// `crate::extensions::types` (always compiled) and re-exported here.
pub use crate::extensions::types::{ExtensionLoadError, LoadedExtension};

/// Result of discovery + load for a set of extension paths.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub extensions: Vec<LoadedExtension>,
    pub errors: Vec<ExtensionLoadError>,
}

/// Default extension discovery paths (upstream `--extension` defaults),
/// ordered project-local BEFORE user-global (the upstream discovery-order
/// fix; combined with CLI paths first this yields CLI → project → global).
///
/// - `<project_dir>/.pi/extensions` — project-local extensions, searched
///   only when the caller declares the project trusted.
/// - `<agent_dir>/extensions` — user-global extensions, always searched.
///
/// The production wiring treats a project as trusted when the
/// `PIR_TRUST_PROJECT=1` environment variable is set; this function
/// deliberately takes the decision as a parameter and never reads the
/// environment, so tests stay hermetic.
pub fn default_extension_paths(
    agent_dir: &Path,
    project_dir: &Path,
    trust_project: bool,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if trust_project {
        paths.push(project_dir.join(".pi").join("extensions"));
    }
    paths.push(agent_dir.join("extensions"));
    paths
}

/// Synthetic QuickJS module name for an extension file path.
pub fn module_name_for(path: &Path) -> String {
    // Use forward slashes and lowercase the drive prefix for stable naming.
    // One buffer: the prefix and the `\` -> `/` rewrite are written directly,
    // where `replace` + `format!` would allocate the rewritten form and then
    // the prefixed form.
    let display = path.to_string_lossy();
    let mut name = String::with_capacity(EXT_MODULE_PREFIX.len() + display.len());
    name.push_str(EXT_MODULE_PREFIX);
    name.extend(display.chars().map(|c| if c == '\\' { '/' } else { c }));
    name
}

/// Absolute, lexically normalized form of `path` (no filesystem access: no
/// symlink resolution, `..` resolved against the spelled path only).
///
/// The paths that reach the loader are user-supplied (`-e ./ext`, a settings
/// entry, a relative `pir js` script) while every allowed root is compared
/// component-wise against them. A relative spelling therefore has to be
/// anchored once, at the boundary: `module_name_to_path` feeds the on-disk
/// resolution with the module name's spelling, and a relative path can never
/// satisfy `starts_with(<root>)` once either side carries a `.`/drive/root
/// component the other lacks — `module_dir` also loses the leading `./` in
/// `normalize_joined`, so a relative entry name is not even self-consistent.
/// Node's module ids are absolute for the same reason.
pub fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize_lexically(path);
    }
    match std::env::current_dir() {
        Ok(cwd) => normalize_lexically(&cwd.join(path)),
        Err(_) => normalize_lexically(path),
    }
}

/// Resolve `.` / `..` components without touching the filesystem. A `..`
/// that has nothing to pop (start of a relative path, or above a root) is
/// kept as-is rather than silently dropped.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None => out.push(".."),
                // `..` above a root is a no-op (Windows and POSIX agree).
                _ => {}
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Transpiled JavaScript for a `pir js` script-mode entry, with the
/// top-level-await downgrade applied (`transpile::transpile_script_entry`).
///
/// Entry classification follows Node 22+ detect-module: an ESM-classified
/// file always goes through the entry transpile (the TLA wrapper is a no-op
/// without top-level await); an ambiguous `.js` that `module_kind` classified
/// CJS but whose *top level* carries an `await` re-parses as ESM (Node's
/// detect-module reparses on the same signal — `await import(...)` alone is
/// valid CJS, so `npm_resolve::has_module_syntax` misses it). CJS entries
/// without top-level await, plus JSON/native kinds, keep their own loader
/// path (`None`). The script runner seeds the result into the module-source
/// cache so the downgrade applies to the entry only; imported modules keep
/// native ESM semantics.
pub fn script_entry_module_js(path: &Path) -> Option<String> {
    // One read for both steps: for an ambiguous `.js` the classification
    // reads the file to detect module syntax and hands the text back here, so
    // the source below is not read a second time; every other spelling pays
    // the single read it always did.
    let (kind, source) = module_kind_and_source(path);
    let source = source.or_else(|| std::fs::read_to_string(path).ok())?;
    match kind {
        npm_resolve::ModuleKind::Esm => {}
        npm_resolve::ModuleKind::Cjs => {
            if !source_has_top_level_await(&source) {
                return None;
            }
        }
        _ => return None,
    }
    let display = path.to_string_lossy();
    super::transpile::transpile_script_entry(&display, &source).ok()
}

/// Lexical scan for an `await` at script top level (depth 0 across
/// `{}/()/[]` nesting, outside strings/templates/comments). An `await`
/// directly after `=>` (async arrow expression body) is excluded — its await
/// belongs to that arrow, not to the module top level. This is the
/// script-entry classification heuristic only; imported modules are never
/// classified through it.
pub(crate) fn source_has_top_level_await(source: &str) -> bool {
    const IDENT_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_$";
    // Byte length bounds the char count (every char is at least one byte), so
    // reserving it up front avoids the reallocations `collect()` performs while
    // growing from the iterator's `len / 4` lower bound.
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
    let mut quote = '"';
    let mut last_word = String::new();
    let mut depth = 0usize;
    let mut prev_char: Option<char> = None;
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
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    mode = Mode::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Mode::Code => {
                match c {
                    '/' => {
                        if chars.get(i + 1) == Some(&'/') {
                            mode = Mode::LineComment;
                            i += 2;
                            continue;
                        }
                        if chars.get(i + 1) == Some(&'*') {
                            mode = Mode::BlockComment;
                            i += 2;
                            continue;
                        }
                        prev_char = Some(c);
                        i += 1;
                    }
                    '"' | '\'' => {
                        mode = Mode::Str;
                        quote = c;
                        i += 1;
                    }
                    '`' => {
                        mode = Mode::Template;
                        i += 1;
                    }
                    '{' | '(' | '[' => {
                        depth += 1;
                        prev_char = Some(c);
                        i += 1;
                    }
                    '}' | ')' | ']' => {
                        depth = depth.saturating_sub(1);
                        prev_char = Some(c);
                        i += 1;
                    }
                    c if IDENT_CHARS.contains(c) => {
                        last_word.push(c);
                        i += 1;
                    }
                    c => {
                        // Word boundary: decide on the completed keyword.
                        if !last_word.is_empty() {
                            let member_access = prev_char == Some('.');
                            if last_word == "await" && depth == 0 && !member_access {
                                // Exclude async arrow expression bodies
                                // (`async () => await x`): the arrow's `=>`.
                                if prev_char != Some('=') {
                                    return true;
                                }
                            }
                            last_word.clear();
                        }
                        if !c.is_whitespace() {
                            prev_char = Some(c);
                        }
                        i += 1;
                    }
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Dependency scanning (upstream loader's purpose: decide what must be
// shimmed and reject node/npm dependencies before execution)
// ---------------------------------------------------------------------------

/// How a dependency reference was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    /// Static `import ... from "spec"` / `export ... from "spec"`.
    StaticImport,
    /// Dynamic `import("spec")`.
    DynamicImport,
    /// CommonJS `require("spec")`.
    Require,
}

/// One dependency reference found by [`scan_dependencies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyRef {
    pub specifier: String,
    pub kind: DependencyKind,
}

impl DependencyRef {
    /// Whether the runtime can serve this specifier: relative paths, absolute
    /// paths and `file:` URLs resolve on disk below an allowed root, and known
    /// bare specifiers resolve to host virtual-module shims. Bare npm
    /// specifiers are allowed through the scan and fail later at load time
    /// with a resolution error; `node:` builtins outside the shimmed table
    /// stay rejected here.
    pub fn is_supported(&self) -> bool {
        if self.specifier.starts_with('.') || path_specifier_to_local(&self.specifier).is_some() {
            return true;
        }
        if super::api::shims::canonical_virtual_name(&self.specifier).is_some() {
            return true;
        }
        !self.specifier.starts_with("node:")
    }
}

/// Local filesystem path for a specifier written as a `file:` URL or as an
/// absolute path (`/x/y`, `C:\x\y`, `C:/x/y`, `\\server\share\x`); `None` for
/// every other spelling (relative and bare specifiers keep their own
/// resolution paths).
///
/// Node's ESM loader accepts `file:` URLs and absolute paths as module
/// specifiers, and upstream extensions rely on both:
/// `import(pathToFileURL(entry).href)` is how pi-subagents loads the host
/// `@earendil-works/pi-coding-agent` package by absolute package entry
/// (`child-session.js` `loadHostPiCodingAgent`) and how it imports an injected
/// child-session factory module (`runner-child-sessions.js`).
pub fn path_specifier_to_local(name: &str) -> Option<PathBuf> {
    if name
        .get(..5)
        .is_some_and(|head| head.eq_ignore_ascii_case("file:"))
    {
        return file_url_to_path(name);
    }
    let bytes = name.as_bytes();
    // POSIX-absolute or Windows drive-rooted ("C:/x", "C:\x"); a Windows
    // UNC path ("\\server\share") keeps both leading separators.
    if bytes.first() == Some(&b'/') {
        return Some(PathBuf::from(name));
    }
    if bytes.starts_with(b"\\\\") {
        return Some(PathBuf::from(name));
    }
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return Some(PathBuf::from(name));
    }
    None
}

/// Path out of a `file:` URL: percent-decoded, with the Windows drive prefix
/// `file:///C:/x` spelled `/C:/x` restored to `C:/x`. A non-local authority
/// (`file://host/x`) and a URL with no absolute path are rejected (`None`),
/// like Node's refusal to load remote file URLs.
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.get(5..)?; // past "file:"
    let path = if let Some(after_slashes) = rest.strip_prefix("//") {
        let (authority, tail) = match after_slashes.find(['/', '\\']) {
            Some(index) => (&after_slashes[..index], &after_slashes[index..]),
            None => (after_slashes, ""),
        };
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        tail
    } else {
        rest
    };
    if !path.starts_with('/') && !path.starts_with('\\') {
        return None;
    }
    let decoded = percent_decode(path);
    Some(PathBuf::from(strip_windows_drive_separator(&decoded)))
}

/// `file:///C:/x` yields the pathname `/C:/x`; Windows wants `C:/x` back.
/// Only a single leading separator before a drive letter is dropped, so a
/// POSIX spelling (`/C:/x` is a directory literally named `C:` there) is
/// untouched on non-Windows hosts.
fn strip_windows_drive_separator(path: &str) -> &str {
    if !cfg!(windows) {
        return path;
    }
    let bytes = path.as_bytes();
    if bytes.len() > 2
        && matches!(bytes[0], b'/' | b'\\')
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        return &path[1..];
    }
    path
}

/// `%XX` byte decoder for URL path components (UTF-8 decoded at the end; an
/// invalid escape is kept verbatim, which is what Node's URL parser does for
/// stray `%`).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &input[i + 1..i + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Scan JavaScript source (typically post-transpile) for module specifiers:
/// static imports and re-exports (including multi-line statements), dynamic
/// `import(...)`, and CommonJS `require(...)`. String literals and comments
/// are skipped so specifiers inside them are not reported.
pub fn scan_dependencies(source: &str) -> Vec<DependencyRef> {
    const IDENT_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_$";

    // Byte length bounds the char count (every char is at least one byte), so
    // reserving it up front avoids the reallocations `collect()` performs while
    // growing from the iterator's `len / 4` lower bound.
    let mut chars: Vec<char> = Vec::with_capacity(source.len());
    chars.extend(source.chars());
    let mut deps: Vec<DependencyRef> = Vec::new();

    #[derive(PartialEq)]
    enum Mode {
        Code,
        Str,
        Template,
        LineComment,
        BlockComment,
    }
    let mut mode = Mode::Code;
    // Stack of template-literal modes suspended by `${ ... }` expressions.
    let mut template_stack: Vec<()> = Vec::new();
    let mut quote = '"';
    let mut last_word = String::new();
    let mut i = 0usize;

    // After `import`/`require`/`from`, parse a quoted specifier that follows
    // (skipping whitespace and, for require/import, the opening paren).
    let parse_specifier =
        |chars: &[char], mut i: usize, need_paren: bool| -> Option<(String, usize)> {
            let mut seen_paren = !need_paren;
            while i < chars.len() {
                let c = chars[i];
                if c.is_whitespace() {
                    i += 1;
                    continue;
                }
                // Comments between the keyword and its specifier are legal
                // syntax and common in bundled dynamic imports
                // (`import(/* webpackChunkName: "x" */ "./x.js")`).
                if c == '/' && i + 1 < chars.len() {
                    if chars[i + 1] == '/' {
                        i += 2;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        continue;
                    }
                    if chars[i + 1] == '*' {
                        i += 2;
                        while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                            i += 1;
                        }
                        i = (i + 2).min(chars.len());
                        continue;
                    }
                }
                if !seen_paren {
                    if c == '(' {
                        seen_paren = true;
                        i += 1;
                        continue;
                    }
                    return None;
                }
                if c == '(' {
                    // `from (` / dynamic `import (` — keep scanning for the
                    // quoted specifier (covers `await import("x")`).
                    i += 1;
                    continue;
                }
                if c == '"' || c == '\'' || c == '`' {
                    let quote = c;
                    i += 1;
                    let mut specifier = String::new();
                    while i < chars.len() {
                        let c = chars[i];
                        if c == '\\' && i + 1 < chars.len() {
                            specifier.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        if c == quote {
                            return Some((specifier, i + 1));
                        }
                        specifier.push(c);
                        i += 1;
                    }
                    return None; // unterminated
                }
                return None;
            }
            None
        };

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
                    i += 1;
                    continue;
                }
                if c == '`' {
                    mode = Mode::Template;
                    last_word.clear();
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
                    i += 1;
                    continue;
                }
                if IDENT_CHARS.contains(c) {
                    last_word.push(c);
                    i += 1;
                    continue;
                }
                // Word boundary: dispatch on the completed keyword.
                if !last_word.is_empty() {
                    match last_word.as_str() {
                        "import" => {
                            if let Some((specifier, next)) = parse_specifier(&chars, i, false) {
                                // `import("x")` vs `import "x"`: both carry a
                                // specifier; disambiguate by what preceded the
                                // quote.
                                let mut j = i;
                                let mut saw_paren = false;
                                while j < next && j < chars.len() {
                                    if chars[j] == '(' {
                                        saw_paren = true;
                                        break;
                                    }
                                    j += 1;
                                }
                                deps.push(DependencyRef {
                                    specifier,
                                    kind: if saw_paren {
                                        DependencyKind::DynamicImport
                                    } else {
                                        DependencyKind::StaticImport
                                    },
                                });
                                i = next;
                                last_word.clear();
                                continue;
                            }
                        }
                        "require" => {
                            if let Some((specifier, next)) = parse_specifier(&chars, i, true) {
                                deps.push(DependencyRef {
                                    specifier,
                                    kind: DependencyKind::Require,
                                });
                                i = next;
                                last_word.clear();
                                continue;
                            }
                        }
                        "from" => {
                            if let Some((specifier, next)) = parse_specifier(&chars, i, false) {
                                deps.push(DependencyRef {
                                    specifier,
                                    kind: DependencyKind::StaticImport,
                                });
                                i = next;
                                last_word.clear();
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                last_word.clear();
                i += 1;
            }
        }
    }

    deps
}

/// Normalize a joined path lexically (resolve `.` and `..`), using `/`
/// separators. Does not touch the filesystem. A root ("/", "C:/") is
/// preserved: module ids are absolute paths, and trimming the leading
/// separator would turn a POSIX absolute import target into a relative one
/// that the allowed-roots check can never accept.
fn normalize_joined(dir: &str, rel: &str) -> String {
    let base = if rel.starts_with('/') {
        String::new()
    } else {
        dir.to_string()
    };
    let combined = format!("{base}/{rel}");
    let mut parts: Vec<&str> = Vec::new();
    for seg in combined.split('/') {
        match seg {
            "" | "." => {
                if parts.is_empty() && seg.is_empty() {
                    parts.push("");
                }
            }
            ".." => {
                // Keep the leading "" (root) and a possible drive segment.
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Extract the file path from a `pir-ext:` module name.
pub fn module_name_to_path(module_name: &str) -> Option<&str> {
    module_name.strip_prefix(EXT_MODULE_PREFIX)
}

/// Directory containing an extension module (empty string for root-level).
fn module_dir(module_name: &str) -> Option<String> {
    let path = module_name_to_path(module_name)?;
    match path.rfind('/') {
        Some(idx) => Some(path[..idx].to_string()),
        None => Some(String::new()),
    }
}

/// Discover extension entry points under the given paths.
///
/// Each path is either a file (loaded directly) or a directory searched with
/// the upstream discovery rules (`loader.ts:discoverExtensionsInDir`):
///
/// 1. a `package.json` `"pi".extensions` manifest wins,
/// 2. then `index.ts`, then `index.js` at the directory root,
/// 3. otherwise scan: top-level `*.ts`/`*.js` files load directly AND each
///    subdirectory resolves through the per-dir entry rules
///    (`<dir>/foo/package.json` manifest → `<dir>/foo/index.ts` →
///    `<dir>/foo/index.js`); no recursion beyond one level.
pub fn discover_extensions(paths: &[PathBuf]) -> DiscoveryResult {
    let mut result = DiscoveryResult::default();
    let mut seen: std::collections::BTreeSet<PathBuf> = Default::default();
    let mut visited_dirs: std::collections::BTreeSet<PathBuf> = Default::default();

    // Anchor every input path before it can reach a synthetic module id or
    // the `seen` set: a relative `-e` spelling must not leak into module
    // names (relative-import resolution and the allowed-roots check both
    // depend on the absolute form; see [`absolute_path`]).
    let anchored: Vec<PathBuf> = paths.iter().map(|path| absolute_path(path)).collect();

    for path in &anchored {
        if path.is_file() {
            push_entry(path.clone(), &mut seen, &mut result);
            continue;
        }
        if !path.is_dir() {
            result.errors.push(ExtensionLoadError {
                path: path.clone(),
                error: format!("not a file or directory: {}", path.display()),
            });
            continue;
        }
        discover_in_dir(path, &mut seen, &mut visited_dirs, &mut result);
    }

    result
}

/// Record one entry point, deduplicated by path.
fn push_entry(
    path: PathBuf,
    seen: &mut std::collections::BTreeSet<PathBuf>,
    result: &mut DiscoveryResult,
) {
    if seen.insert(path.clone()) {
        let module_name = module_name_for(&path);
        result
            .extensions
            .push(LoadedExtension { path, module_name });
    }
}

/// A directory's entry rules: `package.json` `"pi".extensions` manifest wins,
/// then `index.ts`, then `index.js`; with none of those, the top-level
/// `*.ts`/`*.js` files load directly and each subdirectory resolves through
/// [`resolve_subdir_entries`] (one level, no recursion). A manifest target
/// that names a directory re-enters these rules; `visited_dirs` bounds the
/// walk to unique directories.
fn discover_in_dir(
    dir: &Path,
    seen: &mut std::collections::BTreeSet<PathBuf>,
    visited_dirs: &mut std::collections::BTreeSet<PathBuf>,
    result: &mut DiscoveryResult,
) {
    // A manifest entry may name a directory, so directory entries can nest.
    // `visited_dirs` breaks a manifest cycle (`"./"` naming its own root)
    // instead of recursing forever.
    let anchored = absolute_path(dir);
    if !visited_dirs.insert(anchored) {
        return;
    }

    // (1) package.json "pi".extensions manifest
    let manifest = dir.join("package.json");
    if manifest.is_file() {
        match std::fs::read_to_string(&manifest) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json) => {
                    let rels = json
                        .get("pi")
                        .and_then(|pi| pi.get("extensions"))
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    if !rels.is_empty() {
                        add_manifest_entries(dir, &manifest, &rels, seen, visited_dirs, result);
                        return;
                    }
                    // Manifest present but empty: fall through to index rules.
                }
                Err(e) => {
                    result.errors.push(ExtensionLoadError {
                        path: manifest,
                        error: format!("invalid package.json: {e}"),
                    });
                    return;
                }
            },
            Err(e) => {
                result.errors.push(ExtensionLoadError {
                    path: manifest,
                    error: format!("cannot read package.json: {e}"),
                });
                return;
            }
        }
    }

    // (2)/(3) index.ts, then index.js
    for index in ["index.ts", "index.js"] {
        let candidate = dir.join(index);
        if candidate.is_file() {
            push_entry(candidate, seen, result);
            return;
        }
    }

    // (4) scan: top-level *.ts / *.js files, plus one level of subdirectories
    // resolved through the per-dir entry rules (manifest → index.ts →
    // index.js), sorted for determinism.
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                    if ext == "ts" || ext == "js" {
                        entries.push(p);
                    }
                }
            } else if p.is_dir() {
                subdirs.push(p);
            }
        }
    }
    entries.sort();
    for p in entries {
        push_entry(p, seen, result);
    }
    subdirs.sort();
    for subdir in subdirs {
        resolve_subdir_entries(&subdir, seen, visited_dirs, result);
    }
}

/// Record a manifest's `pi.extensions` targets.
///
/// A target may be a file or a directory. Upstream resolves a directory
/// target through its own entry rules (`pi-web-access` declares `"./dist"`
/// with `dist/index.js`; `@vndv/pi-codegraph` declares `"./extensions"` with
/// a top-level `.ts` file), so directories are expanded with
/// [`discover_in_dir`] instead of being reported as missing.
fn add_manifest_entries(
    root: &Path,
    manifest: &Path,
    rels: &[serde_json::Value],
    seen: &mut std::collections::BTreeSet<PathBuf>,
    visited_dirs: &mut std::collections::BTreeSet<PathBuf>,
    result: &mut DiscoveryResult,
) {
    for rel in rels {
        let Some(rel_str) = rel.as_str() else {
            result.errors.push(ExtensionLoadError {
                path: manifest.to_path_buf(),
                error: "pi.extensions entries must be strings".into(),
            });
            continue;
        };
        // `path.resolve` semantics (upstream resolves manifest targets):
        // anchor and lexically normalize so a `./entry.ts` spelling reaches
        // module names without a `.` component.
        let full = absolute_path(&root.join(rel_str));
        if full.is_file() {
            push_entry(full, seen, result);
        } else if full.is_dir() {
            discover_in_dir(&full, seen, visited_dirs, result);
        } else {
            result.errors.push(ExtensionLoadError {
                path: full,
                error: "manifest entry does not exist".into(),
            });
        }
    }
}

/// Per-subdirectory entry rules (upstream `resolveExtensionEntries`):
/// `package.json` `"pi".extensions` manifest → `index.ts` → `index.js`;
/// a subdirectory with none contributes nothing (no recursion).
fn resolve_subdir_entries(
    subdir: &Path,
    seen: &mut std::collections::BTreeSet<PathBuf>,
    visited_dirs: &mut std::collections::BTreeSet<PathBuf>,
    result: &mut DiscoveryResult,
) {
    let manifest = subdir.join("package.json");
    if manifest.is_file() {
        if let Ok(content) = std::fs::read_to_string(&manifest) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                let rels = json
                    .get("pi")
                    .and_then(|pi| pi.get("extensions"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !rels.is_empty() {
                    add_manifest_entries(subdir, &manifest, &rels, seen, visited_dirs, result);
                    return;
                }
            }
        }
    }
    for index in ["index.ts", "index.js"] {
        let candidate = subdir.join(index);
        if candidate.is_file() {
            push_entry(candidate, seen, result);
            return;
        }
    }
}

/// Shared module-source cache so the runtime can seed transpiled sources
/// before importing an extension module.
pub type SourceCache = std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>;

/// Shared allow-list of root directories for on-disk relative imports.
pub type AllowedRoots = std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>;

/// QuickJS `Resolver` + `Loader` pair for the extension runtime.
pub struct ExtensionModuleLoader {
    cache: SourceCache,
    /// Root directories allowed for on-disk module loading.
    allowed_roots: AllowedRoots,
}

impl ExtensionModuleLoader {
    pub fn new(cache: SourceCache, allowed_roots: AllowedRoots) -> Self {
        Self {
            cache,
            allowed_roots,
        }
    }

    fn load_source(&self, name: &str) -> Option<String> {
        // Virtual modules (TypeBox shim, pi package shims).
        if let Some(canonical) = super::api::shims::canonical_virtual_name(name) {
            if let Some(src) = super::api::shims::source_for(canonical) {
                return Some(src.to_string());
            }
        }
        if let Ok(cache) = self.cache.lock() {
            if let Some(src) = cache.get(name) {
                return Some(src.clone());
            }
        }
        // On-demand: resolve the file on disk, then dispatch on its kind.
        if let Some(rel_path) = module_name_to_path(name) {
            if let Some(path) = self.resolve_candidate(Path::new(rel_path)) {
                match module_kind_and_source(&path) {
                    (npm_resolve::ModuleKind::Json, _) => {
                        let source = std::fs::read_to_string(&path).ok()?;
                        return Some(json_default_wrapper(&source));
                    }
                    (npm_resolve::ModuleKind::Cjs, source) => {
                        let actual = path.to_string_lossy().replace('\\', "/");
                        // The lexer reads the file itself, except when the
                        // classification above already did (ambiguous `.js`);
                        // its re-export targets are always read by path.
                        let names = match source {
                            Some(source) => cjs_lexer::named_exports_with_source(&path, source),
                            None => cjs_lexer::named_exports(&path),
                        };
                        return Some(cjs_interop_wrapper(&actual, &names));
                    }
                    (npm_resolve::ModuleKind::Native, _) => {
                        // Resolver::resolve already rejects .node for bare
                        // specifiers; a direct relative .node import still
                        // reports the same clear error here.
                        return Some(
                            "throw new Error(\"native addons (.node) are not supported in this runtime\");"
                                .to_string(),
                        );
                    }
                    (npm_resolve::ModuleKind::Esm, source) => {
                        // Same split as the CJS arm: classification's text for
                        // an ambiguous `.js`, a read here for every other ESM
                        // spelling.
                        let source = match source {
                            Some(source) => source,
                            None => std::fs::read_to_string(&path).ok()?,
                        };
                        // Transpile must report the file that was actually read.
                        let actual = path.to_string_lossy();
                        return transpile(&actual, &source)
                            .map_err(|e| {
                                crate::ext_diag!("pir-ext: transpile failed for {name}: {e}")
                            })
                            .ok();
                    }
                }
            }
        }
        None
    }

    /// First loadable file among the on-disk resolution candidates: exact
    /// path, the TS NodeNext rename (`./x.js` -> `x.ts`, `.mjs` -> `.mts`),
    /// extensionless `+ .ts` / `+ .js`, then directory `index.ts` /
    /// `index.js`. Every candidate passes the allowed-roots check.
    fn resolve_candidate(&self, path: &Path) -> Option<PathBuf> {
        candidate_paths(path)
            .into_iter()
            .find(|candidate| self.is_allowed_file(candidate))
    }

    /// A loadable on-disk file: it exists and stays under an allowed root.
    ///
    /// Both sides are anchored before the component-wise prefix test: the
    /// roots are registered from the caller's raw paths (`runtime.load` takes
    /// the CLI paths verbatim, so `-e ./ext` registers `./ext`) while `path`
    /// arrives spelled inside a module name. Anchoring the pair is what makes
    /// a relative entry usable at all — and it also covers the `pir js`
    /// script runner, which registers a possibly-relative script directory.
    fn is_allowed_file(&self, path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        let candidate = absolute_path(path);
        self.allowed_roots
            .lock()
            .map(|roots| {
                roots
                    .iter()
                    .any(|root| candidate.starts_with(absolute_path(root)))
            })
            .unwrap_or(false)
    }
}

/// Module kind of the file at `path`, plus the file's source text when the
/// classification had to read the file.
///
/// [`npm_resolve::module_kind`] reads the source for exactly one shape: a
/// `.js` whose nearest enclosing `package.json` leaves the syntax ambiguous
/// (no `"type"` field, no manifest at all, or an unusable one), where the
/// source's module syntax decides ESM vs CommonJS. That text is what the
/// loader's branch needs anyway, so it is handed back here instead of being
/// dropped and read again — an ambiguous `.js` is read once, and neither the
/// CJS lexer nor the ESM transpile reads it a second time. Every other shape
/// delegates to `module_kind` unchanged: the kind comes from the extension or
/// the nearest manifest without touching the source, and no text rides along.
///
/// The `.js` walk below must stay in lockstep with `module_kind`
/// (npm_resolve.rs, the `Some("js")` arm); it re-derives only that decision,
/// and [`npm_resolve::has_module_syntax`] stays the syntax scan for both.
///
/// Shared by this loader's on-demand `load_source` dispatch and the runtime's
/// entry seeding ([`crate::extensions::runtime`] `load`), plus the script-entry
/// classification above.
/// Test seam for the runtime's entry classification (`module_kind_and_source`):
/// integration tests need the exact kind/source decision the runtime seeds a
/// module with, without widening the loader's normal API surface.
#[doc(hidden)]
pub fn module_kind_and_source_for_test(path: &Path) -> (npm_resolve::ModuleKind, Option<String>) {
    module_kind_and_source(path)
}

/// Test seam over the loader's relative-import probing: the first existing
/// on-disk candidate for `specifier` as imported by `importer`, using exactly
/// the runtime's NodeNext rules (exact path, `.js`→`.ts`, `.mjs`→`.mts`,
/// `.cjs`→`.cts`, extensionless `+ .ts`/`+ .js`, then `index.ts`/`index.js`).
///
/// Integration tests use this to prove that a scanned relative dependency is
/// actually loadable, instead of treating any `./x` spelling as ingestible.
/// It is read-only and performs no engine work.
#[doc(hidden)]
pub fn resolve_relative_for_test(importer: &Path, specifier: &str) -> Option<PathBuf> {
    let base = importer.parent()?.join(specifier);
    candidate_paths(&base)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

pub(crate) fn module_kind_and_source(path: &Path) -> (npm_resolve::ModuleKind, Option<String>) {
    if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
        return (npm_resolve::module_kind(path), None);
    }
    // Nearest manifest decides; one without a usable `"type"` leaves the file
    // ambiguous and falls through to the syntax scan (same `break` as
    // `module_kind`: an unreadable or invalid manifest is ambiguous, not CJS).
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            let declared = std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|json| {
                    json.get("type")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            match declared.as_deref() {
                Some("module") => return (npm_resolve::ModuleKind::Esm, None),
                Some(_) => return (npm_resolve::ModuleKind::Cjs, None),
                None => break,
            }
        }
        cursor = dir.parent();
    }
    // Ambiguous `.js`: read once, here, and keep the text for the branch. A
    // failed read keeps the Cjs default `module_kind` falls back to and hands
    // back no text, so the branch reads the file exactly as it did before.
    match std::fs::read_to_string(path) {
        Ok(source) => {
            let kind = if npm_resolve::has_module_syntax(&source) {
                npm_resolve::ModuleKind::Esm
            } else {
                npm_resolve::ModuleKind::Cjs
            };
            (kind, Some(source))
        }
        Err(_) => (npm_resolve::ModuleKind::Cjs, None),
    }
}

/// On-disk resolution candidates for a module path, in probe order: the exact
/// path, the TS NodeNext rename (`./x.js` -> `x.ts`, `.mjs` -> `.mts`),
/// extensionless `+ .ts` / `+ .js`, then directory `index.ts` / `index.js`
/// (pure: existence and the allowed-roots gate are checked by the caller).
fn candidate_paths(path: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![path.to_path_buf()];
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js") => candidates.push(path.with_extension("ts")),
        Some("mjs") => candidates.push(path.with_extension("mts")),
        // NodeNext's CommonJS spelling: `./x.cjs` is authored as `x.cts`.
        Some("cjs") => candidates.push(path.with_extension("cts")),
        None => {
            let mut ts = path.as_os_str().to_owned();
            ts.push(".ts");
            candidates.push(PathBuf::from(ts));
            let mut js = path.as_os_str().to_owned();
            js.push(".js");
            candidates.push(PathBuf::from(js));
        }
        _ => {}
    }
    // The directory index candidates are appended unconditionally: callers
    // probe the list lazily and stop at the first hit, so the common file case
    // never looks at them, while an eager `is_dir()` here would cost a
    // filesystem call on every resolution. `<path>/index.ts` can only exist
    // when `path` is a directory, so the candidate set is unchanged.
    candidates.push(path.join("index.ts"));
    candidates.push(path.join("index.js"));
    candidates
}

/// `export default JSON.parse(<escaped text>)` wrapper for a `.json` module:
/// the JSON text is embedded via a serde_json string literal (valid JS).
pub(crate) fn json_default_wrapper(source: &str) -> String {
    let literal = serde_json::to_string(source).unwrap_or_else(|_| "\"\"".to_string());
    format!("export default JSON.parse({literal});\n")
}

/// CommonJS bridge: run the file through `globalThis.__pirCjsLoad` and expose
/// its `module.exports` as the default export plus every statically detected
/// export name as a named export (phase 4b). Named exports are a link-time
/// snapshot of the file's `exports.X = ...` surface — the same semantics as
/// Node's cjs-module-lexer; a file whose source does not parse (or has no
/// detectable names) degrades to the default-only wrapper.
pub(crate) fn cjs_interop_wrapper(abs_path: &str, names: &BTreeSet<String>) -> String {
    let literal = serde_json::to_string(abs_path).unwrap_or_else(|_| "\"\"".to_string());
    let mut out = String::from(
        "// CJS interop: `export default` is module.exports; named exports are a\n\
         // link-time snapshot (same semantics as Node's cjs-module-lexer).\n",
    );
    out.push_str(&format!(
        "const __m = globalThis.__pirCjsLoad({literal});\nexport default __m.exports;\n"
    ));
    for name in names {
        let name_literal = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
        out.push_str(&format!(
            "export const {name} = __m.exports[{name_literal}];\n"
        ));
    }
    out
}

/// Default extra bare-specifier search roots: the user-scope npm root
/// (`~/.pi/agent/npm`, resolved like every other agent-dir consumer via
/// `PIR_AGENT_DIR`/`PI_CODING_AGENT_DIR` env overrides). Extension mode
/// never needs these (extensions live inside the npm tree, so the walk-up
/// finds its own root); standalone `pir js` scripts have no extension
/// directory to derive an npm root from, so the resolver falls back to this
/// list when the `node_modules` walk-up from the script comes up empty
/// (Node's `NODE_PATH` fallback semantics).
pub fn bare_search_roots() -> Vec<PathBuf> {
    vec![crate::coding_agent::context::agent_dir().join("npm")]
}

// ---------------------------------------------------------------------------
// Host pi package (the SDK identity pi-subagents discovers)
// ---------------------------------------------------------------------------

/// Upstream host-SDK package name.
pub const HOST_PI_PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";

/// The env override pi-subagents consults when the host package cannot be
/// found from `process.argv[1]` (`resolvePiPackageRoot`) or
/// `import.meta.resolve` (`resolveInstalledPiPackageRoot`); it then hands the
/// root to the detached runner and loads the SDK entry through it
/// (`async-execution.js` `resolveAsyncPiPackageRoot` / `loadHostPiCodingAgent`).
pub const HOST_PI_PACKAGE_ROOT_ENV: &str = "PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT";

/// Where pir keeps its host pi package: `<agent_dir>/pir/host-pi-package`.
pub fn host_pi_package_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("pir").join("host-pi-package")
}

/// Materialize the host pi package and return its root.
///
/// pi-subagents discovers its host SDK as a *package root*: it reads
/// `<root>/package.json` (name must be `@earendil-works/pi-coding-agent`),
/// resolves the package's root export, imports that entry by `file:` URL, and
/// probes `<root>` (then its ancestors) for the host peer packages
/// (`resolveHostPeerAliases`). pir serves that SDK itself — the
/// `@earendil-works/pi-coding-agent` virtual module — so the package pir
/// materializes holds a one-line re-export entry plus the manifest identity
/// the discovery contract needs, and stub peer packages so the peer probe
/// resolves. Nothing else is written: the real SDK surface is the host module.
///
/// Files are rewritten only when their content differs, so repeat boots are
/// idempotent and never churn mtimes.
pub fn ensure_host_pi_package_root(agent_dir: &Path) -> std::io::Result<PathBuf> {
    let root = host_pi_package_dir(agent_dir);
    for (relative, content) in host_pi_package_files() {
        let path = root.join(relative);
        if std::fs::read_to_string(&path).is_ok_and(|current| current == content) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    Ok(root)
}

/// `<relative path, content>` for every file of the host pi package.
fn host_pi_package_files() -> Vec<(String, String)> {
    const HOST_MANIFEST: &str = r#"{
  "name": "@earendil-works/pi-coding-agent",
  "version": "0.0.0",
  "private": true,
  "type": "module",
  "exports": {
    ".": "./index.js"
  }
}
"#;
    // The host SDK the entry resolves to: the pir virtual module, re-exported
    // so package-identity imports and bare-specifier imports are one module.
    const HOST_ENTRY: &str = r#"// pir host pi package entry.
//
// pi-subagents loads the host SDK by absolute package entry
// (`loadHostPiCodingAgent`: manifest -> root export -> `file:` URL import).
// pir serves that SDK as a host virtual module, so this entry re-exports it:
// importing either this file or "@earendil-works/pi-coding-agent" yields the
// same module instance.
export * from "@earendil-works/pi-coding-agent";
export { default } from "@earendil-works/pi-coding-agent";
"#;
    // Peer stubs: `resolveHostPeerAliases` requires every host peer package to
    // exist with a resolvable export, or it fails the background launch closed.
    // pir serves those peers as virtual modules too, so the stubs are never
    // imported — throwing keeps that honest if one ever is.
    const PEER_STUB: &str = r#"// pir host peer stub: present so pi-subagents' host-package peer probe
// (`resolveHostPeerAliases`) resolves. pir serves this package as a host
// virtual module, so this file is never imported.
throw new Error("pir host peer stub: this package is provided by the pir host runtime");
"#;
    // Chord is required from host 0.85.0 on; the synthetic manifest declares
    // 0.0.0, so it is a pre-chord host and the required peer set stays at the
    // stable host-peer list.
    let peer_manifest = |name: &str, exports: &str| -> String {
        format!(
            "{{\n  \"name\": \"{name}\",\n  \"version\": \"0.0.0\",\n  \"private\": true,\n  \"type\": \"module\",\n  \"exports\": {exports}\n}}\n"
        )
    };
    let subpaths = |pairs: &[&str]| -> String {
        let mut out = String::from("{");
        for (index, pair) in pairs.iter().enumerate() {
            out.push_str(if index == 0 { "\n    " } else { ",\n    " });
            out.push_str(pair);
        }
        out.push_str("\n  }");
        out
    };
    let agent_core_exports = subpaths(&["\".\": \"./index.js\""]);
    let tui_exports = subpaths(&["\".\": \"./index.js\""]);
    let ai_exports = subpaths(&[
        "\".\": \"./index.js\"",
        "\"./compat\": \"./compat.js\"",
        "\"./oauth\": \"./oauth.js\"",
        "\"./providers/all\": \"./providers/all.js\"",
    ]);
    let typebox_exports = subpaths(&[
        "\".\": \"./index.js\"",
        "\"./compile\": \"./compile.js\"",
        "\"./value\": \"./value.js\"",
    ]);
    vec![
        ("package.json".to_string(), HOST_MANIFEST.to_string()),
        ("index.js".to_string(), HOST_ENTRY.to_string()),
        (
            "node_modules/@earendil-works/pi-agent-core/package.json".to_string(),
            peer_manifest("@earendil-works/pi-agent-core", &agent_core_exports),
        ),
        (
            "node_modules/@earendil-works/pi-agent-core/index.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/@earendil-works/pi-tui/package.json".to_string(),
            peer_manifest("@earendil-works/pi-tui", &tui_exports),
        ),
        (
            "node_modules/@earendil-works/pi-tui/index.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/@earendil-works/pi-ai/package.json".to_string(),
            peer_manifest("@earendil-works/pi-ai", &ai_exports),
        ),
        (
            "node_modules/@earendil-works/pi-ai/index.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/@earendil-works/pi-ai/compat.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/@earendil-works/pi-ai/oauth.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/@earendil-works/pi-ai/providers/all.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/typebox/package.json".to_string(),
            peer_manifest("typebox", &typebox_exports),
        ),
        (
            "node_modules/typebox/index.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/typebox/compile.js".to_string(),
            PEER_STUB.to_string(),
        ),
        (
            "node_modules/typebox/value.js".to_string(),
            PEER_STUB.to_string(),
        ),
    ]
}

impl rquickjs::loader::Resolver for ExtensionModuleLoader {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> JsResult<String> {
        use super::api::shims;

        // Virtual modules resolve by exact (canonical) name.
        if let Some(canonical) = shims::canonical_virtual_name(name) {
            return Ok(canonical.to_string());
        }

        if name.starts_with(EXT_MODULE_PREFIX) {
            // Already-normalized extension module name.
            return Ok(name.to_string());
        }

        if name.starts_with('.') {
            // Relative import: resolve against the importing module's dir.
            let dir = module_dir(base).ok_or_else(|| {
                rquickjs::Error::new_loading_message(
                    name,
                    "relative import from a non-extension module",
                )
            })?;
            let joined = normalize_joined(&dir, name);
            return Ok(format!("{EXT_MODULE_PREFIX}{joined}"));
        }

        // Absolute path or `file:` URL: load the file directly, with the
        // allowed-roots gate applied to the absolute target (upstream
        // `import(pathToFileURL(entry).href)` — pi-subagents' host-SDK loader
        // and child-session-factory imports).
        if let Some(target) = path_specifier_to_local(name) {
            let candidates = candidate_paths(&target);
            if let Some(path) = candidates
                .iter()
                .find(|candidate| self.is_allowed_file(candidate))
            {
                return Ok(module_name_for(path));
            }
            let exists = candidates.iter().find(|candidate| candidate.is_file());
            return Err(rquickjs::Error::new_loading_message(
                name,
                match exists {
                    // The reason leads and the name is left to the engine's
                    // own `Error loading module '<name>'` wrapper: long load
                    // messages are truncated.
                    Some(path) => {
                        format!("outside the allowed extension roots ({})", path.display())
                    }
                    None => format!("cannot find module '{name}'"),
                },
            ));
        }

        // `node:` outside the shimmed table keeps the original rejection.
        if name.starts_with("node:") {
            return Err(rquickjs::Error::new_loading_message(
                name,
                "cannot resolve module (only relative imports and known pi packages are supported)",
            ));
        }

        // Bare specifier: npm-style resolution from the importing module's
        // directory. The package root joins the allowed-roots list so the
        // resolved file (and its siblings) pass the on-disk load check.
        let dir = module_dir(base).ok_or_else(|| {
            rquickjs::Error::new_loading_message(
                name,
                "bare import from a module without a directory",
            )
        })?;
        match npm_resolve::resolve_bare_searching(
            Path::new(&dir),
            &bare_search_roots(),
            name,
            npm_resolve::ResolveMode::Esm,
        ) {
            Ok(resolved) => {
                if let Ok(mut roots) = self.allowed_roots.lock() {
                    if !roots.contains(&resolved.package_root) {
                        roots.push(resolved.package_root.clone());
                    }
                }
                Ok(module_name_for(&resolved.path))
            }
            Err(error) => Err(rquickjs::Error::new_loading_message(
                name,
                error.to_string(),
            )),
        }
    }
}

impl rquickjs::loader::Loader for ExtensionModuleLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> JsResult<Module<'js, rquickjs::module::Declared>> {
        let source = self
            .load_source(name)
            .ok_or_else(|| rquickjs::Error::new_loading_message(name, "module source not found"))?;
        let module = Module::declare(ctx.clone(), name, source)?;
        // QuickJS leaves `import.meta.url` unset for loader-declared modules;
        // populate the Node-shaped fields so extensions can compute sibling
        // paths via `new URL(rel, import.meta.url)` (pi-subagents pattern).
        if let Some(path) = module_name_to_path(name).filter(|path| !path.is_empty()) {
            let meta = module.meta()?;
            meta.set("url", module_file_url(path))?;
            meta.set("filename", path.to_string())?;
            meta.set("dirname", module_dirname(path))?;
        }
        Ok(module)
    }
}

/// `file://` URL of a disk-backed module path: forward slashes, a leading
/// slash before drive letters, and minimal percent-encoding per UTF-8 byte.
fn module_file_url(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let mut out = String::from("file://");
    if !normalized.starts_with('/') {
        out.push('/');
    }
    for byte in normalized.bytes() {
        let keep =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Directory part of a module path (empty for a bare file name).
fn module_dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}
