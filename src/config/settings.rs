//! settings.json support: full upstream `core/settings-manager.ts` surface —
//! two-scope (global + project) load and deep merge with the `defaultTools`
//! modifier rule, legacy migrations, parse-error recording with per-field
//! typed diagnostics and upstream-shaped startup diagnostics, trust-gated
//! project scope, field-level scoped
//! persistence with file locking (nested-key granularity, unknown keys
//! preserved round-trip; corrupt files are never overwritten), plus
//! context-file (`AGENTS.md`/`CLAUDE.md`) and `SYSTEM.md`/`APPEND_SYSTEM.md`
//! discovery.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::types::ThinkingLevel;
use crate::config::defaults::settings_defaults as defaults;

mod context_files;
mod diagnostics;
mod env;
mod files;
mod manager;
mod merge;
mod schema;
mod tools;

pub use context_files::*;
pub use diagnostics::*;
pub use env::*;
pub use files::*;
pub use manager::*;
pub use merge::*;
pub use schema::*;
pub use tools::*;
// Internal helpers shared by submodules (private to this module tree).
use files::{canonicalize_lossy, read_text_file, read_text_file_checked, with_file_lock};
use tools::merge_default_tools;

/// The full upstream settings surface. All fields optional; unknown keys are
/// captured in `extra` and preserved on round-trip (upstream
/// `additionalProperties: true`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(
        rename = "lastChangelogVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_changelog_version: Option<String>,
    #[serde(rename = "defaultProvider", skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(rename = "defaultModel", skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(
        rename = "defaultThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_thinking_level: Option<ThinkingLevel>,
    #[serde(
        rename = "modelThinkingLevels",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_thinking_levels: Option<BTreeMap<String, ThinkingLevel>>,
    #[serde(rename = "transport", skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportSetting>,
    #[serde(
        rename = "sessionShareDestination",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_share_destination: Option<SessionShareDestination>,
    #[serde(rename = "steeringMode", skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<SteeringMode>,
    #[serde(rename = "followUpMode", skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<FollowUpMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(rename = "branchSummary", skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<BranchSummarySettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySettings>,
    #[serde(rename = "hideThinkingBlock", skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    #[serde(
        rename = "showCacheMissNotices",
        skip_serializing_if = "Option::is_none"
    )]
    pub show_cache_miss_notices: Option<bool>,
    #[serde(rename = "externalEditor", skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
    #[serde(rename = "shellPath", skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(rename = "quietStartup", skip_serializing_if = "Option::is_none")]
    pub quiet_startup: Option<QuietStartup>,
    #[serde(
        rename = "defaultProjectTrust",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_project_trust: Option<DefaultProjectTrust>,
    #[serde(rename = "shellCommandPrefix", skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    /// `shellPool`: run bash commands through a persistent pooled shell
    /// process instead of spawning a fresh shell per call (pir-only
    /// optimization; absent = on, `false` restores the per-call spawn).
    #[serde(rename = "shellPool", skip_serializing_if = "Option::is_none")]
    pub shell_pool: Option<bool>,
    /// Per-tool execution settings (pir-only `tools` object), currently the
    /// opt-in default timeout for foreground shell commands. Absent = upstream
    /// semantics (no default timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsSettings>,
    #[serde(rename = "npmCommand", skip_serializing_if = "Option::is_none")]
    pub npm_command: Option<Vec<String>>,
    #[serde(rename = "collapseChangelog", skip_serializing_if = "Option::is_none")]
    pub collapse_changelog: Option<bool>,
    #[serde(
        rename = "enableInstallTelemetry",
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_install_telemetry: Option<bool>,
    #[serde(rename = "enableAnalytics", skip_serializing_if = "Option::is_none")]
    pub enable_analytics: Option<bool>,
    #[serde(rename = "trackingId", skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageSource>>,
    /// `defaultDisabledPackagesEnabled` (pir-only, GLOBAL scope): opt in to
    /// the packages pir keeps off by default even when they are installed and
    /// listed in `packages` — currently `@malinamnam/pi-phone`, a local web
    /// server plus ~14 MB of QuickJS heap on every startup (see
    /// `crate::plugins::loader::DEFAULT_DISABLED_PACKAGE_NAMES`). Read via
    /// [`SettingsManager::default_disabled_packages_enabled`], which consults
    /// the global scope only; an explicit CLI `-e/--extension <path>` naming
    /// the package always loads it. Unknown to upstream, whose settings schema
    /// tolerates additional properties, so pir-written settings stay loadable.
    #[serde(
        rename = "defaultDisabledPackagesEnabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_disabled_packages_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(rename = "deviceId", skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Local skill file paths or directories (post-migration array form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(
        rename = "enableSkillCommands",
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_skill_commands: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    #[serde(rename = "enabledModels", skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    #[serde(rename = "defaultTools", skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    #[serde(rename = "doubleEscapeAction", skip_serializing_if = "Option::is_none")]
    pub double_escape_action: Option<DoubleEscapeAction>,
    #[serde(rename = "treeFilterMode", skip_serializing_if = "Option::is_none")]
    pub tree_filter_mode: Option<TreeFilterMode>,
    #[serde(rename = "thinkingBudgets", skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgetsSettings>,
    #[serde(rename = "editorPaddingX", skip_serializing_if = "Option::is_none")]
    pub editor_padding_x: Option<u32>,
    #[serde(rename = "outputPad", skip_serializing_if = "Option::is_none")]
    pub output_pad: Option<u8>,
    #[serde(
        rename = "autocompleteMaxVisible",
        skip_serializing_if = "Option::is_none"
    )]
    pub autocomplete_max_visible: Option<u32>,
    #[serde(rename = "showHardwareCursor", skip_serializing_if = "Option::is_none")]
    pub show_hardware_cursor: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<WarningSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codemode: Option<CodemodeSettings>,
    /// QuickJS resource limits of the extension runtime (pir-only;
    /// `extensionLimits`). Read from the global settings scope only: a
    /// project `settings.json` must never loosen the host's engine limits
    /// (see `crate::extensions::limits`).
    #[serde(rename = "extensionLimits", skip_serializing_if = "Option::is_none")]
    pub extension_limits: Option<ExtensionLimitsSettings>,
    /// Built-in extensions not to load, by name (upstream
    /// `disabledBuiltinExtensions`): `["mcp"]` drops `builtin:mcp` from the
    /// plan even when settings or an explicit `-e builtin:mcp` enable it (the
    /// settings spelling of `--no-mcp`). Hosts read it through
    /// `SettingsManager::disabled_builtins`, which UNIONs the scopes — a
    /// project may add names but never re-enable a builtin the global scope
    /// (or another layer) disabled. Entries are tolerant: a bare string is a
    /// one-name list and junk entries are ignored (see
    /// `schema::deserialize_optional_string_list`).
    #[serde(
        rename = "disabledBuiltinExtensions",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "schema::deserialize_optional_string_list"
    )]
    pub disabled_builtin_extensions: Option<Vec<String>>,
    /// The `pythonTool` switch (pir-only; default on): `false` skips
    /// registering the embedded-RustPython `python` tool (for users who
    /// prefer the native-python path or want to pin their own `python`
    /// extension). Embedders resolve it via
    /// `crate::plugins::builtin::PythonPlugin::with_config`;
    /// `disabledBuiltinExtensions: ["python"]` removes the builtin at the
    /// loader layer regardless.
    #[serde(rename = "pythonTool", skip_serializing_if = "Option::is_none")]
    pub python_tool: Option<bool>,
    #[serde(rename = "sessionDir", skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(rename = "httpProxy", skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,
    #[serde(rename = "httpIdleTimeoutMs", skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<TimeoutSetting>,
    #[serde(rename = "cacheWarming", skip_serializing_if = "Option::is_none")]
    pub cache_warming: Option<CacheWarmingMode>,
    #[serde(
        rename = "websocketConnectTimeoutMs",
        skip_serializing_if = "Option::is_none"
    )]
    pub websocket_connect_timeout_ms: Option<TimeoutSetting>,
    #[serde(rename = "tuiMode", skip_serializing_if = "Option::is_none")]
    pub tui_mode: Option<TuiMode>,
    #[serde(
        rename = "fullscreenExitOutput",
        skip_serializing_if = "Option::is_none"
    )]
    pub fullscreen_exit_output: Option<FullscreenExitOutput>,
    #[serde(
        rename = "fullscreenScrollbar",
        skip_serializing_if = "Option::is_none"
    )]
    pub fullscreen_scrollbar: Option<FullscreenScrollbar>,
    #[serde(
        rename = "fullscreenCopyOnSelect",
        skip_serializing_if = "Option::is_none"
    )]
    pub fullscreen_copy_on_select: Option<bool>,
    #[serde(
        rename = "fullscreenWheelScrollLines",
        skip_serializing_if = "Option::is_none"
    )]
    pub fullscreen_wheel_scroll_lines: Option<FullscreenWheelScrollLines>,
    /// TUI keybinding overrides (upstream keeps these in a separate
    /// `keybindings.json`; pir also honors the legacy `settings.json`
    /// `keybindings` object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keybindings: Option<Value>,
    /// Unknown settings, preserved round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Wheel scroll lines: `"auto"` or a number clamped to 1..=100 by the
/// accessor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FullscreenWheelScrollLines {
    Auto,
    Lines(u64),
}

impl Serialize for FullscreenWheelScrollLines {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            FullscreenWheelScrollLines::Auto => serializer.serialize_str("auto"),
            FullscreenWheelScrollLines::Lines(lines) => serializer.serialize_u64(*lines),
        }
    }
}

impl<'de> Deserialize<'de> for FullscreenWheelScrollLines {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) if text == "auto" => Ok(FullscreenWheelScrollLines::Auto),
            Value::Number(number) => number
                .as_u64()
                .map(FullscreenWheelScrollLines::Lines)
                .ok_or_else(|| {
                    serde::de::Error::custom("expected a non-negative number or \"auto\"")
                }),
            _ => Err(serde::de::Error::custom(
                "expected a non-negative number or \"auto\"",
            )),
        }
    }
}

impl FullscreenWheelScrollLines {
    /// The effective line count, clamped to 1..=100 (upstream
    /// `getFullscreenWheelScrollLines`); `"auto"` yields `None`.
    pub fn effective(self) -> Option<u32> {
        match self {
            FullscreenWheelScrollLines::Auto => None,
            FullscreenWheelScrollLines::Lines(lines) => Some(lines.clamp(1, 100) as u32),
        }
    }
}

impl Settings {
    /// The default settings (upstream `SETTINGS_DEFAULTS` fields that are part
    /// of the schema; unset otherwise).
    pub fn typed_defaults() -> Self {
        Self {
            transport: serde_json::from_value(serde_json::json!(defaults::TRANSPORT)).ok(),
            session_share_destination: serde_json::from_value(serde_json::json!(
                defaults::SESSION_SHARE_DESTINATION
            ))
            .ok(),
            steering_mode: serde_json::from_value(serde_json::json!(defaults::STEERING_MODE)).ok(),
            follow_up_mode: serde_json::from_value(serde_json::json!(defaults::FOLLOW_UP_MODE))
                .ok(),
            ..Self::default()
        }
    }
}
