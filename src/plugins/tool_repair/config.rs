//! The package's config surface: the `PI_TOOL_REPAIR_DEBUG` env var and
//! `agent_dir()/extensions/pi-tool-repair.json`.
//!
//! Upstream anchors: `defaultConfigPath` (`grammar-repair.ts:115-117`),
//! `loadGrammarRepairConfig` (`:99-113`), `normalizeGrammarRepairConfig`
//! (`:120-136`), `normalizeLeakModels` (`:146-153`), and the env-backed debug
//! default (`src/index.ts:25`).
//!
//! Construction is side-effect free: [`ToolRepairConfig::default`] touches
//! neither the env nor the filesystem. [`ToolRepairConfig::load`]/
//! [`ToolRepairConfig::load_from`] are the lazy I/O entry points — Step 1b's
//! plugin wiring calls them (`AgentPlugin::on_init` is the natural spot), so
//! nothing here runs while the builtin stays unregistered.
//!
//! Only the `debug` knob is consumed in Step 1a; the grammar-repair fields are
//! parsed and normalized now so Step 2 can use them without re-porting the
//! loader.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use super::aliases::GRAMMAR_NAMES;

/// `PI_TOOL_REPAIR_DEBUG` (`src/index.ts:25`).
pub const DEBUG_ENV_VAR: &str = "PI_TOOL_REPAIR_DEBUG";

/// The config file name under `<agent dir>/extensions/`.
pub const CONFIG_FILE_NAME: &str = "pi-tool-repair.json";

/// TS `GrammarRepairMode` (`grammar-repair.ts:19`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrammarRepairMode {
    /// Strip leaked markup and append recovered `toolCall` blocks.
    #[default]
    Recover,
    /// Strip leaked markup only.
    Strip,
}

/// TS `GrammarRepairConfig` (`grammar-repair.ts:21-30`) with the package
/// defaults (`:91-97`). Parsed and normalized in Step 1a, used by Step 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarRepairConfig {
    /// Recovery is opt-in (default false).
    pub enabled: bool,
    /// Enabled grammar families; defaults to all of [`GRAMMAR_NAMES`].
    pub grammars: Vec<&'static str>,
    /// `recover` (default) or `strip`.
    pub mode: GrammarRepairMode,
    /// Only recover calls whose name is in the active tool registry.
    pub require_known_tool: bool,
    /// Grammar-repair debug flag (`grammarRepair.debug`).
    pub debug: bool,
    /// Case-insensitive regex fragments that auto-enable recovery per model id
    /// (`leakModels`); invalid fragments are dropped like
    /// `normalizeLeakModels` does.
    pub leak_models: Vec<String>,
}

impl Default for GrammarRepairConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            grammars: GRAMMAR_NAMES.to_vec(),
            mode: GrammarRepairMode::Recover,
            require_known_tool: true,
            debug: false,
            leak_models: Vec::new(),
        }
    }
}

impl GrammarRepairConfig {
    /// The pure normalizer (TS `normalizeGrammarRepairConfig`,
    /// `grammar-repair.ts:120-136`, plus `loadGrammarRepairConfig`'s
    /// `grammarRepair` unwrap, `:103-106`): accepts either the documented
    /// `{"grammarRepair": {...}}` envelope or the bare partial config object
    /// and falls back to the package defaults per missing or ill-typed field.
    pub fn from_json(value: &JsonValue) -> Self {
        let raw = match value.get("grammarRepair") {
            Some(grammar) => grammar,
            None => value,
        };
        let defaults = Self::default();
        let grammars = match raw.get("grammars").and_then(JsonValue::as_array) {
            Some(names) => {
                let known: Vec<&'static str> = names
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .filter_map(|name| GRAMMAR_NAMES.iter().find(|known| **known == name).copied())
                    .collect();
                if known.is_empty() {
                    defaults.grammars.clone()
                } else {
                    known
                }
            }
            None => defaults.grammars.clone(),
        };
        let leak_models = raw
            .get("leakModels")
            .and_then(JsonValue::as_array)
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .filter(|pattern| regex::Regex::new(&format!("(?i){pattern}")).is_ok())
                    .map(str::to_owned)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        Self {
            enabled: raw
                .get("enabled")
                .and_then(JsonValue::as_bool)
                .unwrap_or(defaults.enabled),
            grammars,
            mode: match raw.get("mode").and_then(JsonValue::as_str) {
                Some("strip") => GrammarRepairMode::Strip,
                _ => defaults.mode,
            },
            require_known_tool: raw
                .get("requireKnownTool")
                .and_then(JsonValue::as_bool)
                .unwrap_or(defaults.require_known_tool),
            debug: raw
                .get("debug")
                .and_then(JsonValue::as_bool)
                .unwrap_or(defaults.debug),
            leak_models,
        }
    }
}

/// The builtin's config: the envelope the Step 1a pipeline and the Step 2
/// grammar pass share.
///
/// [`Self::debug`] is the OR the README documents ("Set
/// `PI_TOOL_REPAIR_DEBUG=1` **or** `grammarRepair.debug: true`"); the TS code
/// is narrower — `DEFAULT_CONFIG.debug` is frozen from the env var at module
/// load (`src/index.ts:25`), and the file's `debug` only gates the grammar pass
/// (`grammar-repair.ts:132-136`). The OR is the documented behavior, so it is
/// the ported one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolRepairConfig {
    /// Log repair diagnostics to stderr: the env var or
    /// `grammarRepair.debug: true` (`README` "Debug logging"). This is the
    /// flag the repair pipeline's `debug` parameter takes.
    pub debug: bool,
    /// The grammar-repair block (Step 2).
    pub grammar: GrammarRepairConfig,
}

impl ToolRepairConfig {
    /// `<agent dir>/extensions/pi-tool-repair.json` (TS `defaultConfigPath`;
    /// the agent dir resolution is pir's single-source `agent_dir()`).
    pub fn config_path() -> PathBuf {
        crate::ai::providers::agent_dir()
            .join("extensions")
            .join(CONFIG_FILE_NAME)
    }

    /// Whether `PI_TOOL_REPAIR_DEBUG` is set to a non-empty value — JS
    /// `Boolean(process.env.PI_TOOL_REPAIR_DEBUG)` truthiness
    /// (`src/index.ts:25`).
    pub fn debug_enabled() -> bool {
        std::env::var_os(DEBUG_ENV_VAR).is_some_and(|value| !value.is_empty())
    }

    /// Load the builtin's config: the env-backed debug flag plus the package
    /// config file, if present (TS `DEFAULT_CONFIG.debug` and
    /// `loadGrammarRepairConfig`).
    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    /// [`Self::load`] with an explicit path (tests / callers that already
    /// resolved the agent dir). A missing file is not an error; an unreadable
    /// or unparseable one keeps the defaults and reports the same stderr line
    /// as the TS loader (`grammar-repair.ts:108-112`).
    pub fn load_from(path: &Path) -> Self {
        let grammar = match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<JsonValue>(&raw) {
                Ok(value) => GrammarRepairConfig::from_json(&value),
                Err(error) => {
                    crate::ext_diag!(
                        "[pi-tool-repair] Failed to read grammar repair config at {}: {error}",
                        path.display()
                    );
                    GrammarRepairConfig::default()
                }
            },
            Err(_) => GrammarRepairConfig::default(),
        };
        Self {
            debug: Self::debug_enabled() || grammar.debug,
            grammar,
        }
    }
}
