//! Plugin identity: the upstream `SourceInfo`/`InlineExtension` concept
//! (`packages/coding-agent/src/core/source-info.ts`).
//!
//! Rust builtin plugins and QuickJS JS extensions share ONE identity shape so
//! registry conflict rules, diagnostics, and `source`-filtering behave
//! identically for both.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Prefix of built-in extension paths, such as `builtin:mcp` (upstream
/// `BUILTIN_PATH_PREFIX`).
pub const BUILTIN_PATH_PREFIX: &str = "builtin:";

/// Identity of one plugin in the unified list (upstream
/// `SourceInfo`, reduced to the fields pir's hosts read: name, display path,
/// and the replaceable flag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    /// Plugin name. For builtins this is the `builtin:<name>` name; for path
    /// plugins the file stem; for the JS runtime adapter a fixed name.
    pub name: String,
    /// Display path: `builtin:<name>` for builtins, the file path for path
    /// plugins, `<inline:js>` for the JS runtime adapter.
    pub path: String,
    /// A replaceable plugin is dropped when a non-replaceable plugin
    /// registers the same tool/command/flag name (upstream
    /// `omitReplacedExtensions`).
    pub replaceable: bool,
}

impl PluginInfo {
    /// Identity of a compiled builtin addressed as `builtin:<name>` (upstream
    /// `createSyntheticSourceInfo(path, { source: "builtin", scope:
    /// "temporary" })`; builtins are hidden from `/plugins`-style listings
    /// like upstream's `extension.hidden`).
    pub fn builtin(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            path: format!("{BUILTIN_PATH_PREFIX}{name}"),
            replaceable: false,
        }
    }

    /// Identity of a plugin loaded from an explicit CLI `-e` path.
    pub fn cli_path(path: &Path) -> Self {
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            name,
            path: path.to_string_lossy().replace('\\', "/"),
            replaceable: false,
        }
    }

    /// Identity of an inline (compiled-in factory list) plugin, upstream
    /// `<inline:<name>>`.
    pub fn inline(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            path: format!("<inline:{name}>"),
            replaceable: false,
        }
    }

    /// The synthetic-path source (`builtin` for `builtin:<name>`, upstream
    /// `getSyntheticPathSource`); `None` for file paths.
    pub fn synthetic_source(path: &str) -> Option<String> {
        if path.starts_with(BUILTIN_PATH_PREFIX) {
            return Some("builtin".to_owned());
        }
        if path.starts_with('<') && path.ends_with('>') {
            return path
                .trim_start_matches('<')
                .trim_end_matches('>')
                .split(':')
                .next()
                .filter(|source| !source.is_empty())
                .map(String::from);
        }
        None
    }
}

/// Whether a path string addresses a compiled builtin (`builtin:<name>`).
pub fn is_builtin_path(path: &str) -> bool {
    path.starts_with(BUILTIN_PATH_PREFIX)
}

/// The name part of a `builtin:<name>` path.
pub fn builtin_name(path: &str) -> Option<&str> {
    path.strip_prefix(BUILTIN_PATH_PREFIX)
        .filter(|name| !name.is_empty())
}
