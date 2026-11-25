//! Static tables from pi-tool-repair's `DEFAULT_CONFIG`
//! (`src/index.ts:16-97`): the field-alias table, the string-arg tool table,
//! the model regex sets, and the grammar family list.
//!
//! Nothing here reads the package's config file; the file only carries the
//! grammar-repair knobs ([`super::config`]). Like the TS `DEFAULT_CONFIG`,
//! these tables are hardcoded.

use std::sync::LazyLock;

use regex::Regex;

// ---------------------------------------------------------------------------
// Field aliases (`src/index.ts:36-88`)
// ---------------------------------------------------------------------------

/// One canonical field and the alias spellings models use for it
/// (TS `fieldAliases[tool][canonical] = aliasNames`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldAlias {
    /// The canonical (schema-declared) field name.
    pub canonical: &'static str,
    /// Aliases in match order; the first present, usable alias wins.
    pub aliases: &'static [&'static str],
}

/// The shared path-alias list (`read`/`write`/`edit`).
const PATH_ALIASES: &[&str] = &[
    "absolutePath",
    "file_path",
    "filePath",
    "filepath",
    "pathname",
    "target_file",
    "targetFile",
    "file",
    "absolute_path",
    "fileAbsolutePath",
];

/// The `ls` path list: the shared path aliases plus directory spellings.
const LS_PATH_ALIASES: &[&str] = &[
    "absolutePath",
    "file_path",
    "filePath",
    "filepath",
    "pathname",
    "target_file",
    "targetFile",
    "file",
    "absolute_path",
    "fileAbsolutePath",
    "directory",
    "dir",
    "folder",
    "directoryPath",
];

const READ_ALIASES: &[FieldAlias] = &[
    FieldAlias {
        canonical: "path",
        aliases: PATH_ALIASES,
    },
    FieldAlias {
        canonical: "offset",
        aliases: &["start"],
    },
    FieldAlias {
        canonical: "limit",
        aliases: &["max"],
    },
];

#[cfg(feature = "grep-find")]
const GREP_ALIASES: &[FieldAlias] = &[
    FieldAlias {
        canonical: "pattern",
        aliases: &["query", "regex", "search", "q", "expression", "text"],
    },
    FieldAlias {
        canonical: "glob",
        aliases: &["globPattern"],
    },
    FieldAlias {
        canonical: "ignoreCase",
        aliases: &["ic", "caseInsensitive"],
    },
    FieldAlias {
        canonical: "context",
        aliases: &["ctx"],
    },
    FieldAlias {
        canonical: "limit",
        aliases: &["max"],
    },
];

const WRITE_ALIASES: &[FieldAlias] = &[
    FieldAlias {
        canonical: "path",
        aliases: PATH_ALIASES,
    },
    FieldAlias {
        canonical: "content",
        aliases: &["text", "body", "data", "contents", "fileContent"],
    },
];

const EDIT_ALIASES: &[FieldAlias] = &[
    FieldAlias {
        canonical: "path",
        aliases: PATH_ALIASES,
    },
    FieldAlias {
        canonical: "oldText",
        aliases: &[
            "old_string",
            "oldString",
            "old",
            "old_str",
            "oldStr",
            "from",
            "old_value",
            "old_text",
            "oldContent",
            "old_content",
        ],
    },
    FieldAlias {
        canonical: "newText",
        aliases: &[
            "new_string",
            "newString",
            "new",
            "replacement",
            "new_str",
            "newStr",
            "to",
            "new_value",
            "new_text",
            "newContent",
            "new_content",
        ],
    },
];

const LS_ALIASES: &[FieldAlias] = &[
    FieldAlias {
        canonical: "path",
        aliases: LS_PATH_ALIASES,
    },
    FieldAlias {
        canonical: "limit",
        aliases: &["max"],
    },
];

#[cfg(feature = "grep-find")]
const FIND_ALIASES: &[FieldAlias] = &[
    FieldAlias {
        canonical: "pattern",
        aliases: &[
            "query",
            "regex",
            "glob",
            "expression",
            "search",
            "include",
            "name",
            "filename",
        ],
    },
    FieldAlias {
        canonical: "limit",
        aliases: &["max"],
    },
];

const BASH_ALIASES: &[FieldAlias] = &[FieldAlias {
    canonical: "command",
    aliases: &["cmd", "shell", "cmdline", "script", "commandLine"],
}];

/// The curated alias table for one tool (TS `DEFAULT_CONFIG.fieldAliases[tool]`,
/// an empty slice when the tool has none). `grep`/`find` are compiled out
/// with the `grep-find` feature, so their rows disappear with the tools
/// (no call can name a tool this build does not register).
pub fn field_aliases(tool_name: &str) -> &'static [FieldAlias] {
    match tool_name {
        "read" => READ_ALIASES,
        #[cfg(feature = "grep-find")]
        "grep" => GREP_ALIASES,
        "write" => WRITE_ALIASES,
        "edit" => EDIT_ALIASES,
        "ls" => LS_ALIASES,
        #[cfg(feature = "grep-find")]
        "find" => FIND_ALIASES,
        "bash" => BASH_ALIASES,
        _ => &[],
    }
}

/// The `edit` legacy-text canonical names: `oldText` first, then its aliases
/// (TS `repairObjectFromSchema`'s `oldNames`, `src/index.ts:736-737`).
pub fn legacy_old_text_names() -> Vec<&'static str> {
    legacy_names("oldText")
}

/// The `edit` legacy-text canonical names for `newText`
/// (`src/index.ts:737`).
pub fn legacy_new_text_names() -> Vec<&'static str> {
    legacy_names("newText")
}

fn legacy_names(canonical: &'static str) -> Vec<&'static str> {
    let mut names = vec![canonical];
    if let Some(alias) = field_aliases("edit")
        .iter()
        .find(|alias| alias.canonical == canonical)
    {
        names.extend(
            alias
                .aliases
                .iter()
                .copied()
                .filter(|name| *name != canonical),
        );
    }
    names
}

// ---------------------------------------------------------------------------
// String-arg tools (`src/index.ts:89-96`)
// ---------------------------------------------------------------------------

/// How [`super::repair::wrap_root_string_as_object`] wraps a bare string root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapShape {
    /// `{ field: value }` (TS `shape: "string"`).
    Str,
    /// `{ field: [value] }` (TS `shape: "array"`).
    Array,
}

/// One `stringArgTools` entry (TS `{field, shape}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringArgTool {
    /// The schema field the bare string is interpreted as.
    pub field: &'static str,
    /// The field's declared shape in the live schema.
    pub shape: WrapShape,
}

const STRING_ARG_TOOLS: &[(&str, StringArgTool)] = &[
    (
        "grep",
        StringArgTool {
            field: "pattern",
            shape: WrapShape::Str,
        },
    ),
    (
        "find",
        StringArgTool {
            field: "pattern",
            shape: WrapShape::Str,
        },
    ),
    (
        "bash",
        StringArgTool {
            field: "command",
            shape: WrapShape::Str,
        },
    ),
    (
        "read",
        StringArgTool {
            field: "path",
            shape: WrapShape::Str,
        },
    ),
    (
        "ls",
        StringArgTool {
            field: "path",
            shape: WrapShape::Str,
        },
    ),
    (
        "fabric_exec",
        StringArgTool {
            field: "code",
            shape: WrapShape::Str,
        },
    ),
];

/// The bare-string mapping for one tool (TS `DEFAULT_CONFIG.stringArgTools[tool]`).
///
/// Every entry in the TS table is `shape: "string"`, so [`WrapShape::Array`] is
/// not reachable through this table; it is kept because the TS type allows it
/// and the wrap function branches on it.
pub fn string_arg_tool(tool_name: &str) -> Option<&'static StringArgTool> {
    STRING_ARG_TOOLS
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, tool)| tool)
}

// ---------------------------------------------------------------------------
// Model regex sets (`src/index.ts:26-35, 101-109`)
// ---------------------------------------------------------------------------

/// `anchorBleedModels` patterns (`src/index.ts:26-32`), matched
/// case-insensitively against the model id.
pub const ANCHOR_BLEED_MODEL_PATTERNS: &[&str] = &[
    "kimi[-_]?k2",
    "moonshotai/kimi",
    "@cf/.*kimi",
    "minimax",
    "glm",
];

/// `grammarLeakModels` patterns (`src/index.ts:33-35`): the GLM token-leak gate.
pub const GRAMMAR_LEAK_MODEL_PATTERNS: &[&str] = &["glm"];

static ANCHOR_BLEED_MODEL_REGEXES: LazyLock<Vec<Regex>> =
    LazyLock::new(|| compile_models(ANCHOR_BLEED_MODEL_PATTERNS));

static GRAMMAR_LEAK_MODEL_REGEXES: LazyLock<Vec<Regex>> =
    LazyLock::new(|| compile_models(GRAMMAR_LEAK_MODEL_PATTERNS));

fn compile_models(patterns: &[&str]) -> Vec<Regex> {
    // The static patterns are known-good; an uncompilable one is skipped like
    // the TS `catch` in `compileLeakModelPattern` (`grammar-repair.ts:138-144`).
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(&format!("(?i){pattern}")).ok())
        .collect()
}

/// TS `hasAnchorBleedBug` (`src/index.ts:101-104`): true when the model id
/// matches any `anchorBleedModels` pattern. `None` (no model) is always false.
pub fn has_anchor_bleed_bug(model: Option<&str>) -> bool {
    let Some(model) = model.filter(|id| !id.is_empty()) else {
        return false;
    };
    ANCHOR_BLEED_MODEL_REGEXES
        .iter()
        .any(|regex| regex.is_match(model))
}

/// TS `hasGrammarLeakBug` (`src/index.ts:106-109`): true when the model id
/// matches any `grammarLeakModels` pattern. `None` (no model) is always false.
pub fn has_grammar_leak_bug(model: Option<&str>) -> bool {
    let Some(model) = model.filter(|id| !id.is_empty()) else {
        return false;
    };
    GRAMMAR_LEAK_MODEL_REGEXES
        .iter()
        .any(|regex| regex.is_match(model))
}

// ---------------------------------------------------------------------------
// Grammar families (Step 2)
// ---------------------------------------------------------------------------

/// The ten grammar families the package understands
/// (`grammar-repair.ts:5-16`, `GRAMMAR_NAMES`). Consumed by
/// [`super::config::GrammarRepairConfig`] now; the recovery pass itself is
/// Step 2.
pub const GRAMMAR_NAMES: &[&str] = &[
    "dsml",
    "invoke",
    "qwen",
    "kimi",
    "mistral",
    "llama",
    "glm",
    "granite",
    "minimax-text",
    "olmo",
];
