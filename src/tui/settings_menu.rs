//! Interactive `/settings` menu — port of upstream
//! `modes/interactive/components/settings-selector.ts` (the
//! `SettingsSelectorComponent` + `SettingsList` pairing): a DynamicBorder
//! framed list of setting rows, each cycling through its allowed values with
//! `←/→` (upstream `SettingsList` cycle rule), a one-level submenu for the
//! warning toggles (upstream `WarningSettingsSubmenu`), and immediate
//! write-back through [`crate::config::settings::SettingsManager`] (upstream
//! `settingsManager.setXxx` setters with their global-scope persistence).
//!
//! Scope notes vs upstream: the per-model thinking-level stepped submenu, the
//! automatic light/dark theme submenu, and the fullscreen wheel-scroll value
//! list are upstream-only (pir has no `getSupportedThinkingLevels` registry
//! surface here, the auto-theme pairing lives behind `parse_auto_theme`
//! internal to theme.rs, and wheel lines keep the raw stored value). Type-to-
//! search on the list is also upstream-only; pir's other selector overlays do
//! not filter either.

use std::sync::Mutex;

use serde_json::Value as JsonValue;

use super::core::{Component, Container, DynamicBorder, Spacer, Text};
use super::theme::{with_theme, ThemeColor};

/// What one value change asks the host to do (upstream `SettingsCallbacks`).
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    /// Persist one settings field to the global scope. `id` is the row id
    /// (unique per row) mapped onto its settings.json key path by
    /// [`settings_path_for`] — a top-level field (`doubleEscapeAction`) or a
    /// nested key inside an object field (`terminal.showImages`) — upstream
    /// `settingsManager.setXxx` → `markModified`/persist.
    Set { id: &'static str, value: JsonValue },
    /// Apply the theme live in addition to the settings write (upstream
    /// `onThemeChange` → `themeController.setThemeSetting`).
    SetTheme(String),
    /// Set (or clear, with `level: None`) one model's default thinking level
    /// (upstream `onModelThinkingLevelChange` /
    /// `onModelThinkingLevelRemove` → `modelThinkingLevels` in settings).
    SetModelThinking {
        provider: String,
        model: String,
        level: Option<String>,
    },
}

/// One mutable row (upstream `SettingItem`): id, label, description, the
/// allowed display values, the current index, and the JSON payload each
/// display value maps to.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsItem {
    /// The row id (unique; mapped to a settings.json key path by
    /// [`settings_path_for`]).
    pub id: &'static str,
    /// The display label.
    pub label: String,
    /// The description line shown under the selected row.
    pub description: String,
    /// The allowed display values; `payloads[index]` is what `values[index]`
    /// persists.
    pub values: Vec<String>,
    /// Index into `values` of the current display value.
    pub current: usize,
    /// JSON value persisted for `values[index]`.
    pub payloads: Vec<JsonValue>,
    /// `true` when Enter opens the submenu instead of cycling.
    pub submenu: bool,
}

impl SettingsItem {
    fn current_text(&self) -> &str {
        self.values
            .get(self.current)
            .map(String::as_str)
            .unwrap_or("-")
    }
}

fn bool_item(
    id: &'static str,
    label: &str,
    description: &str,
    current: Option<bool>,
    default: bool,
) -> SettingsItem {
    let effective = current.unwrap_or(default);
    SettingsItem {
        id,
        label: label.to_string(),
        description: description.to_string(),
        values: vec!["true".to_string(), "false".to_string()],
        current: if effective { 0 } else { 1 },
        payloads: vec![JsonValue::Bool(true), JsonValue::Bool(false)],
        submenu: false,
    }
}

fn enum_item(
    id: &'static str,
    label: &str,
    description: &str,
    values: &[&str],
    payloads: Vec<JsonValue>,
    current_index: usize,
) -> SettingsItem {
    SettingsItem {
        id,
        label: label.to_string(),
        description: description.to_string(),
        values: values.iter().map(|value| value.to_string()).collect(),
        current: current_index.min(values.len().saturating_sub(1)),
        payloads,
        submenu: false,
    }
}

fn index_of<T: PartialEq>(values: &[T], current: Option<T>) -> usize {
    current
        .and_then(|value| values.iter().position(|candidate| *candidate == value))
        .unwrap_or(0)
}

/// One model the per-model thinking override submenu offers (upstream
/// `config.availableDefaultModels`): the provider/id pair the settings key is
/// built from plus the supported thinking ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelThinkingOption {
    pub provider: String,
    pub id: String,
    /// Supported level wire names in ladder order (`["off"]` for a
    /// non-reasoning model).
    pub levels: Vec<String>,
}

/// Context the menu rows need beyond the raw settings (upstream
/// `SettingsConfig` constructor inputs).
#[derive(Debug, Clone, Default)]
pub struct SettingsMenuContext {
    /// The active theme name (settings theme or the applied default).
    pub current_theme: Option<String>,
    /// Theme names available for [`super::theme::set_theme`].
    pub available_themes: Vec<String>,
    /// Models the per-model thinking submenu lists (upstream
    /// `availableDefaultModels`). Empty hides nothing: the row still opens and
    /// reports "No models available", exactly like upstream.
    pub models: Vec<ModelThinkingOption>,
    /// Configured per-model overrides (`settings.modelThinkingLevels`,
    /// keyed `provider/id`), as level wire names.
    pub model_thinking_levels: std::collections::BTreeMap<String, String>,
    /// The global default thinking level shown in the override descriptions.
    pub default_thinking_level: String,
}

/// Build the mutable rows from the effective settings snapshot, in upstream
/// `SettingsSelectorComponent` item order. The "Show images" and "Image
/// width" rows are gated on terminal capability detection like upstream
/// ("Only show image toggle if terminal supports it",
/// settings-selector.ts:759-776): they appear when [`super::terminal_image`]
/// resolves an inline image protocol — detected (Kitty/iTerm2 terminals,
/// including Herdr, DRIFT D21) or forced by `terminal.images` /
/// `PI_IMAGE_PROTOCOL` — and are absent otherwise. Pure; unit-tested.
pub fn settings_items(
    settings: &crate::config::settings::Settings,
    context: &SettingsMenuContext,
) -> Vec<SettingsItem> {
    use crate::config::settings as cfg;

    let terminal = settings.terminal.clone().unwrap_or_default();
    let images = settings.images.clone().unwrap_or_default();
    let markdown = settings.markdown.clone().unwrap_or_default();
    // Upstream computes this from the (overridden) cached capabilities;
    // pir resolves the same value from the settings' `terminal` overrides
    // without a process-global cache.
    let supports_images = super::terminal_image::supports_images(settings.terminal.as_ref());

    let mut items = vec![
        // upstream "autocompact"
        bool_item(
            "autocompact",
            "Auto-compact",
            "Automatically compact context when it gets too large",
            settings.compaction.as_ref().and_then(|c| c.enabled),
            true,
        ),
        // upstream "show-images"
        bool_item(
            "show-images",
            "Show images",
            "Render images inline in terminal",
            terminal.show_images,
            true,
        ),
        // upstream "image-width-cells"
        enum_item(
            "image-width-cells",
            "Image width",
            "Preferred inline image width in terminal cells",
            &["60", "80", "120"],
            vec![JsonValue::from(60u32), JsonValue::from(80u32), JsonValue::from(120u32)],
            match terminal.image_width_cells {
                Some(80) => 1,
                Some(120) => 2,
                // 60 (and the unset default, upstream
                // `SETTINGS_DEFAULTS.terminal.imageWidthCells`).
                _ => 0,
            },
        ),
        // upstream "auto-resize-images"
        bool_item(
            "auto-resize-images",
            "Auto-resize images",
            "Resize large images to 2000x2000 max for better model compatibility",
            images.auto_resize,
            true,
        ),
        // upstream "block-images"
        bool_item(
            "block-images",
            "Block images",
            "Prevent images from being sent to LLM providers",
            images.block_images,
            false,
        ),
        // upstream "skill-commands"
        bool_item(
            "skill-commands",
            "Skill commands",
            "Register skills as /skill:name commands",
            settings.enable_skill_commands,
            true,
        ),
        // upstream "steering-mode"
        enum_item(
            "steering-mode",
            "Steering mode",
            "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
            &["one-at-a-time", "all"],
            vec![
                serde_json::to_value(cfg::SteeringMode::OneAtATime).unwrap_or_default(),
                serde_json::to_value(cfg::SteeringMode::All).unwrap_or_default(),
            ],
            index_of(
                &[cfg::SteeringMode::OneAtATime, cfg::SteeringMode::All],
                settings.steering_mode,
            ),
        ),
        // upstream "follow-up-mode"
        enum_item(
            "follow-up-mode",
            "Follow-up mode",
            "Alt+Enter queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
            &["one-at-a-time", "all"],
            vec![
                serde_json::to_value(cfg::FollowUpMode::OneAtATime).unwrap_or_default(),
                serde_json::to_value(cfg::FollowUpMode::All).unwrap_or_default(),
            ],
            index_of(
                &[cfg::FollowUpMode::OneAtATime, cfg::FollowUpMode::All],
                settings.follow_up_mode,
            ),
        ),
        // upstream "transport"
        enum_item(
            "transport",
            "Transport",
            "Preferred transport for providers that support multiple transports",
            &["sse", "websocket", "websocket-cached", "auto"],
            vec![
                serde_json::to_value(cfg::TransportSetting::Sse).unwrap_or_default(),
                serde_json::to_value(cfg::TransportSetting::Websocket).unwrap_or_default(),
                serde_json::to_value(cfg::TransportSetting::WebsocketCached).unwrap_or_default(),
                serde_json::to_value(cfg::TransportSetting::Auto).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::TransportSetting::Sse,
                    cfg::TransportSetting::Websocket,
                    cfg::TransportSetting::WebsocketCached,
                    cfg::TransportSetting::Auto,
                ],
                settings.transport,
            ),
        ),
        // upstream "share-destination" (`cea257439`: the row follows
        // "transport" and cycles auto → gist → radius)
        enum_item(
            "share-destination",
            "Session share destination",
            "Where to upload sessions with /share (auto prefers Radius when authenticated, otherwise GitHub Gist)",
            &["auto", "gist", "radius"],
            vec![
                serde_json::to_value(cfg::SessionShareDestination::Auto).unwrap_or_default(),
                serde_json::to_value(cfg::SessionShareDestination::Gist).unwrap_or_default(),
                serde_json::to_value(cfg::SessionShareDestination::Radius).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::SessionShareDestination::Auto,
                    cfg::SessionShareDestination::Gist,
                    cfg::SessionShareDestination::Radius,
                ],
                settings.session_share_destination,
            ),
        ),
        // upstream "http-idle-timeout" (reduced choice list)
        {
            let current = settings
                .http_idle_timeout_ms
                .and_then(cfg::TimeoutSetting::effective_ms);
            let choices: [(u64, &str); 5] = [
                (5_000, "5000"),
                (10_000, "10000"),
                (30_000, "30000"),
                (60_000, "60000"),
                (300_000, "300000"),
            ];
            let position = current
                .and_then(|ms| choices.iter().position(|(value, _)| *value == ms))
                .unwrap_or(2);
            enum_item(
                "http-idle-timeout",
                "HTTP idle timeout",
                "Maximum idle gap while waiting for HTTP headers or body chunks",
                &["5000", "10000", "30000", "60000", "300000"],
                choices
                    .iter()
                    .map(|(ms, _)| JsonValue::from(*ms))
                    .collect(),
                position,
            )
        },
        // upstream "cache-warming-mode"
        enum_item(
            "cache-warming-mode",
            "Cache warming",
            "off; streaming while the agent runs; idle also between runs while continuation stays profitable",
            &["off", "streaming", "idle"],
            vec![
                serde_json::to_value(cfg::CacheWarmingMode::Off).unwrap_or_default(),
                serde_json::to_value(cfg::CacheWarmingMode::Streaming).unwrap_or_default(),
                serde_json::to_value(cfg::CacheWarmingMode::Idle).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::CacheWarmingMode::Off,
                    cfg::CacheWarmingMode::Streaming,
                    cfg::CacheWarmingMode::Idle,
                ],
                settings.cache_warming,
            ),
        ),
        // upstream "hide-thinking"
        bool_item(
            "hide-thinking",
            "Hide thinking",
            "Hide thinking blocks in assistant responses",
            settings.hide_thinking_block,
            false,
        ),
        // upstream "mermaid-rendering"
        enum_item(
            "mermaid-rendering",
            "Mermaid diagrams",
            "Render Mermaid code blocks as Unicode diagrams",
            &["off", "final", "streaming"],
            vec![
                serde_json::to_value(cfg::MermaidRenderingMode::Off).unwrap_or_default(),
                serde_json::to_value(cfg::MermaidRenderingMode::Final).unwrap_or_default(),
                serde_json::to_value(cfg::MermaidRenderingMode::Streaming).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::MermaidRenderingMode::Off,
                    cfg::MermaidRenderingMode::Final,
                    cfg::MermaidRenderingMode::Streaming,
                ],
                markdown.mermaid,
            ),
        ),
        // upstream "cache-miss-notices"
        bool_item(
            "cache-miss-notices",
            "Cache miss notices",
            "Show transcript notices for cache costs and provider recovery diagnostics",
            settings.show_cache_miss_notices,
            true,
        ),
        // upstream "collapse-changelog"
        bool_item(
            "collapse-changelog",
            "Collapse changelog",
            "Show condensed changelog after updates",
            settings.collapse_changelog,
            false,
        ),
        // upstream "quiet-startup"
        {
            let current = match &settings.quiet_startup {
                Some(cfg::QuietStartup::Header) => 1,
                Some(cfg::QuietStartup::Enabled(true)) => 0,
                _ => 2,
            };
            enum_item(
                "quiet-startup",
                "Quiet startup",
                "Disable verbose printing at startup (header: keep only the startup header)",
                &["true", "header", "false"],
                vec![
                    JsonValue::Bool(true),
                    JsonValue::String("header".to_string()),
                    JsonValue::Bool(false),
                ],
                current,
            )
        },
        // upstream "install-telemetry"
        bool_item(
            "install-telemetry",
            "Install telemetry",
            "Send an anonymous version/update ping after changelog-detected updates",
            settings.enable_install_telemetry,
            true,
        ),
        // upstream "default-project-trust"
        enum_item(
            "default-project-trust",
            "Default project trust",
            "Fallback behavior when no extension or saved trust decision decides project trust",
            &["ask", "always", "never"],
            vec![
                serde_json::to_value(cfg::DefaultProjectTrust::Ask).unwrap_or_default(),
                serde_json::to_value(cfg::DefaultProjectTrust::Always).unwrap_or_default(),
                serde_json::to_value(cfg::DefaultProjectTrust::Never).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::DefaultProjectTrust::Ask,
                    cfg::DefaultProjectTrust::Always,
                    cfg::DefaultProjectTrust::Never,
                ],
                settings.default_project_trust,
            ),
        ),
        // upstream "double-escape-action"
        enum_item(
            "double-escape-action",
            "Double-escape action",
            "Action when pressing Escape twice with empty editor",
            &["tree", "fork", "none"],
            vec![
                serde_json::to_value(cfg::DoubleEscapeAction::Tree).unwrap_or_default(),
                serde_json::to_value(cfg::DoubleEscapeAction::Fork).unwrap_or_default(),
                serde_json::to_value(cfg::DoubleEscapeAction::None).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::DoubleEscapeAction::Tree,
                    cfg::DoubleEscapeAction::Fork,
                    cfg::DoubleEscapeAction::None,
                ],
                settings.double_escape_action,
            ),
        ),
        // upstream "tree-filter-mode"
        enum_item(
            "tree-filter-mode",
            "Tree filter mode",
            "Default filter when opening /tree",
            &["default", "no-tools", "user-only", "labeled-only", "all"],
            vec![
                serde_json::to_value(cfg::TreeFilterMode::Default).unwrap_or_default(),
                serde_json::to_value(cfg::TreeFilterMode::NoTools).unwrap_or_default(),
                serde_json::to_value(cfg::TreeFilterMode::UserOnly).unwrap_or_default(),
                serde_json::to_value(cfg::TreeFilterMode::LabeledOnly).unwrap_or_default(),
                serde_json::to_value(cfg::TreeFilterMode::All).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::TreeFilterMode::Default,
                    cfg::TreeFilterMode::NoTools,
                    cfg::TreeFilterMode::UserOnly,
                    cfg::TreeFilterMode::LabeledOnly,
                    cfg::TreeFilterMode::All,
                ],
                settings.tree_filter_mode,
            ),
        ),
        // upstream "warnings" submenu (kept; pir carries the same single
        // toggle inside the nested store)
        SettingsItem {
            id: "warnings",
            label: "Warnings".to_string(),
            description: "Enable or disable individual warnings".to_string(),
            values: vec!["configure".to_string()],
            current: 0,
            payloads: vec![JsonValue::Null],
            submenu: true,
        },
        // upstream "tui-mode"
        enum_item(
            "tui-mode",
            "TUI mode",
            "Interface layout; regular mode uses the terminal's normal scrollback",
            &["regular", "fullscreen"],
            vec![
                serde_json::to_value(cfg::TuiMode::Regular).unwrap_or_default(),
                serde_json::to_value(cfg::TuiMode::Fullscreen).unwrap_or_default(),
            ],
            index_of(&[cfg::TuiMode::Regular, cfg::TuiMode::Fullscreen], settings.tui_mode),
        ),
        // upstream "fullscreen-exit-output"
        enum_item(
            "fullscreen-exit-output",
            "Fullscreen exit output",
            "Print the transcript or only a session resume hint when exiting fullscreen mode",
            &["transcript", "resume-hint"],
            vec![
                serde_json::to_value(cfg::FullscreenExitOutput::Transcript).unwrap_or_default(),
                serde_json::to_value(cfg::FullscreenExitOutput::ResumeHint).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::FullscreenExitOutput::Transcript,
                    cfg::FullscreenExitOutput::ResumeHint,
                ],
                settings.fullscreen_exit_output,
            ),
        ),
        // upstream "fullscreen-scrollbar"
        enum_item(
            "fullscreen-scrollbar",
            "Fullscreen scrollbar",
            "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
            &["auto", "always", "hidden"],
            vec![
                serde_json::to_value(cfg::FullscreenScrollbar::Auto).unwrap_or_default(),
                serde_json::to_value(cfg::FullscreenScrollbar::Always).unwrap_or_default(),
                serde_json::to_value(cfg::FullscreenScrollbar::Hidden).unwrap_or_default(),
            ],
            index_of(
                &[
                    cfg::FullscreenScrollbar::Auto,
                    cfg::FullscreenScrollbar::Always,
                    cfg::FullscreenScrollbar::Hidden,
                ],
                settings.fullscreen_scrollbar,
            ),
        ),
        // upstream "fullscreen-copy-on-select"
        bool_item(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
            settings.fullscreen_copy_on_select,
            true,
        ),
        // upstream "show-hardware-cursor"
        bool_item(
            "show-hardware-cursor",
            "Show hardware cursor",
            "Use the terminal cursor instead of Pi's drawn cursor",
            settings.show_hardware_cursor,
            false,
        ),
        // upstream "editor-padding"
        enum_item(
            "editor-padding",
            "Editor padding",
            "Horizontal padding for input editor (0-3)",
            &["0", "1", "2", "3"],
            (0u32..=3)
                .map(JsonValue::from)
                .collect(),
            settings.editor_padding_x.unwrap_or(0) as usize % 4,
        ),
        // upstream "output-padding"
        enum_item(
            "output-padding",
            "Output padding",
            "Horizontal padding for messages, tool output, and command output",
            &["0", "1"],
            vec![JsonValue::from(0u8), JsonValue::from(1u8)],
            // Upstream `SETTINGS_DEFAULTS.outputPad` is 1; `0` is the only
            // other valid value.
            settings.output_pad.unwrap_or(1) as usize % 2,
        ),
        // upstream "autocomplete-max-visible"
        enum_item(
            "autocomplete-max-visible",
            "Autocomplete max items",
            "Max visible items in autocomplete dropdown (3-20)",
            &["3", "5", "7", "10", "15", "20"],
            [3u32, 5, 7, 10, 15, 20]
                .into_iter()
                .map(JsonValue::from)
                .collect(),
            [3u32, 5, 7, 10, 15, 20]
                .iter()
                .position(|value| Some(*value) == settings.autocomplete_max_visible)
                .unwrap_or(3),
        ),
        // upstream "clear-on-shrink"
        bool_item(
            "clear-on-shrink",
            "Clear on shrink",
            "Clear empty rows when content shrinks (may cause flicker)",
            settings.terminal.as_ref().and_then(|terminal| terminal.clear_on_shrink),
            false,
        ),
        // upstream "terminal-progress"
        bool_item(
            "terminal-progress",
            "Terminal progress",
            "Show OSC 9;4 progress indicators in the terminal tab bar",
            settings.terminal.as_ref().and_then(|terminal| terminal.show_terminal_progress),
            false,
        ),
        // upstream "fullscreen-wheel-scroll-lines" (always-available numeric
        // list; `auto` accelerates fast wheel spins). W11-C adds this gap row
        // and the per-model-thinking row just before "theme", keeping the
        // pre-existing prefix order (pinned by the parity suite) intact.
        {
            let current = settings
                .fullscreen_wheel_scroll_lines
                .map(|value| match value {
                    crate::config::settings::FullscreenWheelScrollLines::Auto => "auto".to_string(),
                    crate::config::settings::FullscreenWheelScrollLines::Lines(lines) => {
                        lines.to_string()
                    }
                })
                .unwrap_or_else(|| "auto".to_string());
            let mut values: Vec<String> = vec![
                "auto".to_string(),
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "5".to_string(),
                "10".to_string(),
            ];
            // A stored value outside the preset list (settings.json can say
            // any 1..=100) is spliced in so the row shows the truth instead
            // of silently snapping to a preset.
            if !values.iter().any(|value| value == &current) {
                values.push(current.clone());
            }
            let payloads = values
                .iter()
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map(JsonValue::from)
                        .unwrap_or_else(|_| JsonValue::String("auto".to_string()))
                })
                .collect();
            let index = values
                .iter()
                .position(|value| value == &current)
                .unwrap_or(0);
            SettingsItem {
                id: "fullscreen-wheel-scroll-lines",
                label: "Fullscreen wheel scrolling".to_string(),
                description: "Lines per mouse-wheel event in fullscreen mode; 'auto' speeds up fast wheel spins where the terminal does not".to_string(),
                values,
                current: index,
                payloads,
                submenu: false,
            }
        },
        // upstream "model-thinking" (Default thinking level per model). The
        // stepped submenu (model → level, plus "(clear override)") is opened
        // with Enter; the summary value lists the configured overrides.
        {
            let overrides: Vec<String> = context
                .model_thinking_levels
                .iter()
                .map(|(model, level)| format!("{model}={level}"))
                .collect();
            let summary = if overrides.is_empty() {
                "none".to_string()
            } else if overrides.len() <= 2 {
                overrides.join(", ")
            } else {
                format!("{} overrides", overrides.len())
            };
            SettingsItem {
                id: "model-thinking",
                label: "Default thinking level per model".to_string(),
                description: "Override the default thinking level for specific models. The thinking cycle key switches it in-session; Esc backs out of the level list.".to_string(),
                values: vec![summary],
                current: 0,
                payloads: vec![JsonValue::Null],
                submenu: true,
            }
        },
        // upstream "theme" (the submenu offers the single-theme list plus the
        // automatic light/dark pair, upstream `ThemeSubmenu`).
        {
            let themes = if context.available_themes.is_empty() {
                vec!["dark".to_string(), "light".to_string()]
            } else {
                context.available_themes.clone()
            };
            let automatic = context
                .current_theme
                .as_deref()
                .and_then(super::theme::parse_auto_theme_setting)
                .is_some();
            let current = if automatic {
                // The submenu's first row is the mode toggle.
                themes.len()
            } else {
                context
                    .current_theme
                    .as_ref()
                    .and_then(|name| themes.iter().position(|candidate| candidate == name))
                    .unwrap_or(0)
            };
            SettingsItem {
                id: "theme",
                label: "Theme".to_string(),
                description: "Color theme for the interface, or automatic light/dark following the terminal appearance".to_string(),
                values: themes,
                current,
                payloads: Vec::new(),
                submenu: true,
            }
        },
    ];
    // The theme row's payloads follow its value list (single themes; the
    // automatic mode is a submenu-only choice, see `theme_submenu_rows`).
    if let Some(theme) = items.iter_mut().find(|item| item.id == "theme") {
        theme.payloads = theme
            .values
            .iter()
            .map(|name| JsonValue::String(name.clone()))
            .collect();
    }
    // Upstream splices "show-images" and "image-width-cells" in only when the
    // terminal supports images; dropping them here keeps every other row in
    // upstream order.
    if !supports_images {
        items.retain(|item| item.id != "show-images" && item.id != "image-width-cells");
    }
    items
}

/// The thinking levels a model supports, as wire names in ladder order
/// (upstream `getSupportedThinkingLevels`): `["off"]` for a non-reasoning
/// model; otherwise the extended ladder minus levels the model's
/// `thinkingLevelMap` maps to `null`, and minus `xhigh`/`max` when the map
/// does not name them.
pub fn thinking_levels_for_model(model: &crate::ai::types::Model) -> Vec<String> {
    const LADDER: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    if !model.reasoning {
        return vec!["off".to_string()];
    }
    LADDER
        .iter()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(**level));
            match mapped {
                Some(None) => false,
                None => !matches!(**level, "xhigh" | "max"),
                Some(Some(_)) => true,
            }
        })
        .map(|level| (*level).to_string())
        .collect()
}

/// The warning submenu rows (upstream `WarningSettingsSubmenu` items).
fn warning_items(warnings: &crate::config::settings::WarningSettings) -> Vec<SettingsItem> {
    let current = warnings.anthropic_extra_usage.unwrap_or(true);
    vec![bool_item(
        "anthropic-extra-usage",
        "Anthropic extra usage",
        "Warn when Anthropic subscription auth may use paid extra usage",
        Some(current),
        true,
    )]
}

/// Menu keys, decoupled from crossterm for direct tests (the TUI maps real
/// key events in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    /// Type-to-search (upstream `SettingsList` with `enableSearch`): appends
    /// to the fuzzy filter query.
    Char(char),
    /// Deletes one character of the filter query.
    Backspace,
}

/// What a key press produced.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuOutcome {
    /// Key not consumed.
    None,
    /// A value changed (or a submenu choice did); apply the action.
    Apply(SettingsAction),
    /// The main menu is closed.
    Close,
}

/// The interactive settings menu overlay state (pure; the host executes
/// [`SettingsAction`]s against the live settings store).
pub struct SettingsMenu {
    state: Mutex<MenuState>,
}

/// Which submenu a frame is (the stepped submenus need to know what Enter
/// means; a plain value list just cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmenuKind {
    /// The warning toggles (upstream `WarningSettingsSubmenu`).
    Warnings,
    /// The single-theme list + the automatic-mode entry (upstream
    /// `ThemeSubmenu.showSingleMenu`).
    Theme,
    /// Automatic light/dark rows (upstream `ThemeSubmenu.showAutomaticMenu`).
    ThemeAutomatic,
    /// The model list of the per-model thinking submenu.
    Models,
    /// The thinking ladder of `models[model_index]` plus "(clear override)".
    ModelLevels { model_index: usize },
}

struct Submenu {
    kind: SubmenuKind,
    rows: Vec<SettingsItem>,
    selected: usize,
}

struct MenuState {
    items: Vec<SettingsItem>,
    selected: usize,
    /// Fuzzy filter over the main list's labels (empty = everything).
    query: String,
    /// Open submenu, or `None` for the main list.
    submenu: Option<Submenu>,
    /// The prebuilt warnings rows (submenu payload).
    warnings_rows: Vec<SettingsItem>,
    /// The settings snapshot inputs submenu builders need.
    context: SettingsMenuContext,
    /// The raw `theme` settings value (`light/dark` pair or a single name).
    theme_setting: Option<String>,
    /// The current per-model overrides, updated as the levels are applied.
    model_thinking_levels: std::collections::BTreeMap<String, String>,
    /// Last known default thinking level (for the clear-override label).
    default_thinking_level: String,
}

/// The fuzzy-visible row indices for `query`, best match first (upstream
/// `SettingsList.applyFilter` over `fuzzyFilter(items, query, label)`).
fn visible_indices(rows: &[SettingsItem], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..rows.len()).collect();
    }
    let needle = query.to_lowercase();
    let mut scored: Vec<(usize, f64)> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let haystack = row.label.to_lowercase();
            let hit = super::fuzzy::fuzzy_match(&needle, &haystack);
            hit.matches.then_some((index, hit.score))
        })
        .collect();
    scored.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    scored.into_iter().map(|(index, _)| index).collect()
}

impl SettingsMenu {
    /// Build the menu from the effective settings snapshot and theme context.
    pub fn new(settings: &crate::config::settings::Settings, context: SettingsMenuContext) -> Self {
        let warnings = settings.warnings.clone().unwrap_or_default();
        let mut state = MenuState {
            items: settings_items(settings, &context),
            selected: 0,
            query: String::new(),
            submenu: None,
            warnings_rows: warning_items(&warnings),
            context,
            theme_setting: settings.theme.clone(),
            model_thinking_levels: std::collections::BTreeMap::new(),
            default_thinking_level: String::new(),
        };
        state.model_thinking_levels = state
            .context
            .model_thinking_levels
            .iter()
            .map(|(model, level)| (model.clone(), level.clone()))
            .collect();
        state.default_thinking_level = state.context.default_thinking_level.clone();
        Self {
            state: Mutex::new(state),
        }
    }

    /// The row descriptions + current values, main menu then submenu rows
    /// when open (test/render seam).
    pub fn rows(&self) -> Vec<(String, String)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (rows, visible) = state.display_rows();
        visible
            .iter()
            .filter_map(|index| rows.get(*index))
            .map(|item| (item.label.clone(), item.current_text().to_string()))
            .collect()
    }

    /// The fuzzy filter query (empty when not searching).
    pub fn query(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .query
            .clone()
    }

    /// The label of the highlighted row (test seam).
    pub fn selected_label(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (rows, _) = state.display_rows();
        state.current_row(rows).map(|item| item.label.clone())
    }

    /// W11-C main-screen mouse: the visible list row a click on frame row
    /// `row` targets, and whether it moved the highlight. Frame rows before
    /// the list: border, spacer, title, [search line], spacer (see
    /// [`Component::render`]).
    pub fn click_row(&self, row: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let header = if state.query.is_empty() { 4 } else { 5 };
        let Some(offset) = row.checked_sub(header) else {
            return false;
        };
        const MAX_VISIBLE: usize = 10;
        let next = {
            let (rows, visible) = state.display_rows();
            let selected = state
                .current_index(rows)
                .and_then(|current| visible.iter().position(|index| *index == current))
                .unwrap_or(0);
            let start = selected
                .saturating_sub(MAX_VISIBLE / 2)
                .min(visible.len().saturating_sub(MAX_VISIBLE.min(visible.len())));
            if offset >= MAX_VISIBLE || start + offset >= visible.len() {
                return false;
            }
            visible[start + offset]
        };
        state.set_current(next);
        true
    }

    fn step(state: &mut MenuState, delta: isize) {
        let next = {
            let (rows, visible) = state.display_rows();
            if visible.is_empty() {
                return;
            }
            let current = state.current_index(rows);
            let position = visible
                .iter()
                .position(|index| Some(*index) == current)
                .unwrap_or(0);
            let next_position = if delta < 0 {
                position.saturating_sub(delta.unsigned_abs())
            } else {
                (position + delta as usize).min(visible.len() - 1)
            };
            visible[next_position]
        };
        state.set_current(next);
    }

    fn cycle(state: &mut MenuState, delta: isize) -> MenuOutcome {
        let index = {
            let (rows, _) = state.display_rows();
            match state.current_index(rows) {
                Some(index) => index,
                None => return MenuOutcome::None,
            }
        };
        let item = match &mut state.submenu {
            Some(submenu) => submenu.rows.get_mut(index),
            None => state.items.get_mut(index),
        };
        let Some(item) = item else {
            return MenuOutcome::None;
        };
        if item.values.len() < 2 {
            return MenuOutcome::None;
        }
        item.current = if delta < 0 {
            item.current.checked_sub(1).unwrap_or(item.values.len() - 1)
        } else {
            (item.current + 1) % item.values.len()
        };
        MenuOutcome::Apply(item_action(item))
    }

    /// Handle one key (pure; unit-tested). On the main list `Enter` opens a
    /// submenu (warnings / theme / per-model thinking) or cycles right;
    /// printable keys build the type-to-search query; `Esc` clears the query,
    /// then backs out of one submenu level, then closes the menu.
    pub fn handle_key(&self, key: MenuKey) -> MenuOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match key {
            MenuKey::Up => {
                Self::step(&mut state, -1);
                MenuOutcome::None
            }
            MenuKey::Down => {
                Self::step(&mut state, 1);
                MenuOutcome::None
            }
            MenuKey::Left => Self::cycle(&mut state, -1),
            MenuKey::Right => Self::cycle(&mut state, 1),
            MenuKey::Char(c) => {
                state.query.push(c);
                state.reset_selection();
                MenuOutcome::None
            }
            MenuKey::Backspace => {
                state.query.pop();
                state.reset_selection();
                MenuOutcome::None
            }
            MenuKey::Enter => Self::enter(&mut state),
            MenuKey::Esc => {
                if !state.query.is_empty() {
                    state.query.clear();
                    state.reset_selection();
                    return MenuOutcome::None;
                }
                if let Some(submenu) = state.submenu.take() {
                    // Keep the toggled warning values for the next open.
                    if submenu.kind == SubmenuKind::Warnings {
                        state.warnings_rows = submenu.rows;
                    }
                    return MenuOutcome::None;
                }
                MenuOutcome::Close
            }
        }
    }

    fn enter(state: &mut MenuState) -> MenuOutcome {
        let (index, id, is_submenu, level_text) = {
            let (rows, _) = state.display_rows();
            let Some(index) = state.current_index(rows) else {
                return MenuOutcome::None;
            };
            let Some(item) = rows.get(index) else {
                return MenuOutcome::None;
            };
            (
                index,
                item.id,
                item.submenu,
                item.current_text().to_string(),
            )
        };
        let submenu = state.submenu.as_ref().map(|submenu| submenu.kind);
        match submenu {
            None => {
                if is_submenu {
                    let opened = state.build_submenu(id);
                    state.submenu = opened;
                    return MenuOutcome::None;
                }
                Self::cycle(state, 1)
            }
            Some(SubmenuKind::Warnings) => Self::cycle(state, 1),
            Some(SubmenuKind::Theme) => {
                if id == "theme-automatic-mode" {
                    state.submenu = Some(state.theme_automatic_submenu());
                    MenuOutcome::None
                } else {
                    Self::cycle(state, 1)
                }
            }
            Some(SubmenuKind::ThemeAutomatic) => match id {
                "theme-apply" => {
                    let pair = state.automatic_theme_pair();
                    state.theme_setting = Some(pair.clone());
                    state.submenu = None;
                    MenuOutcome::Apply(SettingsAction::SetTheme(pair))
                }
                "theme-change-mode" => {
                    state.submenu = Some(state.theme_submenu());
                    MenuOutcome::None
                }
                _ => Self::cycle(state, 1),
            },
            Some(SubmenuKind::Models) => {
                // The model rows map 1:1 onto `context.models` (plus the
                // "No models available" placeholder).
                if index < state.context.models.len() {
                    state.submenu = Some(state.model_levels_submenu(index));
                }
                MenuOutcome::None
            }
            Some(SubmenuKind::ModelLevels { model_index }) => {
                let Some(model) = state.context.models.get(model_index).cloned() else {
                    state.submenu = Some(state.models_submenu());
                    return MenuOutcome::None;
                };
                let level = (id != "model-thinking-clear").then_some(level_text);
                if let Some(level) = &level {
                    state
                        .model_thinking_levels
                        .insert(model_key(&model), level.clone());
                } else {
                    state.model_thinking_levels.remove(&model_key(&model));
                }
                state.update_model_thinking_summary();
                state.submenu = Some(state.models_submenu());
                MenuOutcome::Apply(SettingsAction::SetModelThinking {
                    provider: model.provider,
                    model: model.id,
                    level,
                })
            }
        }
    }
}

/// `provider/id`, the `modelThinkingLevels` settings key (upstream
/// `modelSettingKey`).
fn model_key(model: &ModelThinkingOption) -> String {
    format!("{}/{}", model.provider, model.id)
}

impl MenuState {
    /// The rows the selection walks (`submenu` or the main list) and the
    /// fuzzy-visible indices within them.
    fn display_rows(&self) -> (&[SettingsItem], Vec<usize>) {
        let rows: &[SettingsItem] = match &self.submenu {
            Some(submenu) => &submenu.rows,
            None => &self.items,
        };
        // Type-to-search filters the MAIN list only: a submenu's rows are a
        // short, hand-built set and upstream's stepped submenus clear the
        // query on entry.
        let visible = match &self.submenu {
            Some(_) => (0..rows.len()).collect(),
            None => visible_indices(rows, &self.query),
        };
        (rows, visible)
    }

    fn current_index(&self, rows: &[SettingsItem]) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        let index = match &self.submenu {
            Some(submenu) => submenu.selected,
            None => self.selected,
        };
        Some(index.min(rows.len() - 1))
    }

    fn current_row<'a>(&self, rows: &'a [SettingsItem]) -> Option<&'a SettingsItem> {
        self.current_index(rows).and_then(|index| rows.get(index))
    }

    fn set_current(&mut self, index: usize) {
        match &mut self.submenu {
            Some(submenu) => submenu.selected = index,
            None => self.selected = index,
        }
    }

    fn reset_selection(&mut self) {
        let first = {
            let (_, visible) = self.display_rows();
            visible.first().copied().unwrap_or(0)
        };
        self.set_current(first);
    }

    /// Open the submenu the row with `id` owns.
    fn build_submenu(&mut self, id: &str) -> Option<Submenu> {
        // Upstream's stepped submenus start with an empty search field.
        self.query.clear();
        match id {
            "warnings" => Some(Submenu {
                kind: SubmenuKind::Warnings,
                rows: self.warnings_rows.clone(),
                selected: 0,
            }),
            // Upstream `ThemeSubmenu`'s constructor opens the AUTOMATIC step
            // directly when the stored setting is a light/dark pair
            // (`this.mode === "automatic" ? showAutomaticMenu()`),
            // settings-selector.ts:282-298.
            "theme" => Some(
                if self.theme_setting.as_deref().is_some_and(|setting| {
                    super::theme::parse_auto_theme_setting(setting).is_some()
                }) {
                    self.theme_automatic_submenu()
                } else {
                    self.theme_submenu()
                },
            ),
            "model-thinking" => Some(self.models_submenu()),
            _ => None,
        }
    }

    fn theme_names(&self) -> Vec<String> {
        if self.context.available_themes.is_empty() {
            vec!["dark".to_string(), "light".to_string()]
        } else {
            self.context.available_themes.clone()
        }
    }

    /// The single-theme list plus the automatic-mode entry (upstream
    /// `ThemeSubmenu.showSingleMenu`).
    fn theme_submenu(&self) -> Submenu {
        let mut rows: Vec<SettingsItem> = self
            .theme_names()
            .into_iter()
            .map(|name| SettingsItem {
                id: "theme",
                label: name.clone(),
                description:
                    "Select a theme, or choose automatic to follow the terminal appearance."
                        .to_string(),
                values: vec![name.clone()],
                current: 0,
                payloads: vec![JsonValue::String(name)],
                submenu: false,
            })
            .collect();
        rows.push(SettingsItem {
            id: "theme-automatic-mode",
            label: "Automatic (follow terminal appearance)".to_string(),
            description: "Use one theme for light terminals and another for dark terminals."
                .to_string(),
            values: vec!["automatic".to_string()],
            current: 0,
            payloads: vec![JsonValue::Null],
            submenu: false,
        });
        let selected = self
            .theme_setting
            .as_deref()
            .map(|setting| {
                if super::theme::parse_auto_theme_setting(setting).is_some() {
                    rows.len() - 1
                } else {
                    rows.iter()
                        .position(|row| row.label == setting)
                        .unwrap_or(0)
                }
            })
            .unwrap_or(0);
        Submenu {
            kind: SubmenuKind::Theme,
            rows,
            selected,
        }
    }

    /// Light/dark pickers, Apply and Change mode (upstream
    /// `ThemeSubmenu.showAutomaticMenu`).
    fn theme_automatic_submenu(&self) -> Submenu {
        let themes = self.theme_names();
        let (light, dark) = self
            .theme_setting
            .as_deref()
            .and_then(super::theme::parse_auto_theme_setting)
            .unwrap_or_else(|| ("light".to_string(), "dark".to_string()));
        let index_of = |name: &str, fallback: &str| {
            themes
                .iter()
                .position(|candidate| candidate == name)
                .or_else(|| themes.iter().position(|candidate| candidate == fallback))
                .unwrap_or(0)
        };
        let rows = vec![
            SettingsItem {
                id: "light-theme",
                label: "Light theme".to_string(),
                description: "Theme to use in automatic mode when the terminal is light"
                    .to_string(),
                values: themes.clone(),
                current: index_of(&light, "light"),
                payloads: themes
                    .iter()
                    .map(|name| JsonValue::String(name.clone()))
                    .collect(),
                submenu: false,
            },
            SettingsItem {
                id: "dark-theme",
                label: "Dark theme".to_string(),
                description: "Theme to use in automatic mode when the terminal is dark".to_string(),
                values: themes.clone(),
                current: index_of(&dark, "dark"),
                payloads: themes
                    .iter()
                    .map(|name| JsonValue::String(name.clone()))
                    .collect(),
                submenu: false,
            },
            SettingsItem {
                id: "theme-apply",
                label: "Apply".to_string(),
                description: "Save and go back".to_string(),
                values: vec!["save and go back".to_string()],
                current: 0,
                payloads: vec![JsonValue::Null],
                submenu: false,
            },
            SettingsItem {
                id: "theme-change-mode",
                label: "Change mode".to_string(),
                description: "Switch to one theme for light and dark".to_string(),
                values: vec!["switch to single theme".to_string()],
                current: 0,
                payloads: vec![JsonValue::Null],
                submenu: false,
            },
        ];
        Submenu {
            kind: SubmenuKind::ThemeAutomatic,
            rows,
            selected: 0,
        }
    }

    /// The `"light/dark"` setting built from the automatic rows.
    fn automatic_theme_pair(&self) -> String {
        let Some(submenu) = self
            .submenu
            .as_ref()
            .filter(|submenu| submenu.kind == SubmenuKind::ThemeAutomatic)
        else {
            return self
                .theme_setting
                .clone()
                .unwrap_or_else(|| "light/dark".to_string());
        };
        let value = |id: &str, fallback: &str| {
            submenu
                .rows
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.current_text().to_string())
                .unwrap_or_else(|| fallback.to_string())
        };
        format!(
            "{}/{}",
            value("light-theme", "light"),
            value("dark-theme", "dark")
        )
    }

    /// The per-model thinking model list (upstream `SteppedSubmenu` step 1).
    fn models_submenu(&self) -> Submenu {
        let mut rows: Vec<SettingsItem> = self
            .context
            .models
            .iter()
            .map(|model| {
                let key = model_key(model);
                let current = self
                    .model_thinking_levels
                    .get(&key)
                    .map(|level| format!(" (override: {level})"))
                    .unwrap_or_default();
                SettingsItem {
                    id: "model-thinking-model",
                    label: format!("{}{current}", key),
                    description: if model.levels.len() > 1 {
                        format!("Supported levels: {}", model.levels.join(", "))
                    } else {
                        "This model does not support thinking".to_string()
                    },
                    values: vec!["configure".to_string()],
                    current: 0,
                    payloads: vec![JsonValue::Null],
                    submenu: true,
                }
            })
            .collect();
        if rows.is_empty() {
            rows.push(SettingsItem {
                id: "model-thinking-none",
                label: "No models available".to_string(),
                description: "Log in to a provider or configure an API key first".to_string(),
                values: vec!["none".to_string()],
                current: 0,
                payloads: vec![JsonValue::Null],
                submenu: false,
            });
        }
        Submenu {
            kind: SubmenuKind::Models,
            rows,
            selected: 0,
        }
    }

    /// The thinking ladder for one model plus "(clear override)" (upstream
    /// `SteppedSubmenu` step 2).
    fn model_levels_submenu(&self, model_index: usize) -> Submenu {
        let model = &self.context.models[model_index];
        let key = model_key(model);
        let active = self.model_thinking_levels.get(&key).cloned();
        let mut rows: Vec<SettingsItem> = model
            .levels
            .iter()
            .map(|level| SettingsItem {
                id: "model-thinking-level",
                label: if Some(level) == active.as_ref() {
                    format!("✓ {level}")
                } else {
                    level.clone()
                },
                description: thinking_level_description(level),
                values: vec![level.clone()],
                current: 0,
                payloads: vec![JsonValue::String(level.clone())],
                submenu: false,
            })
            .collect();
        if active.is_some() {
            rows.push(SettingsItem {
                id: "model-thinking-clear",
                label: "(clear override)".to_string(),
                description: format!(
                    "Revert to global default ({})",
                    if self.default_thinking_level.is_empty() {
                        "unset"
                    } else {
                        &self.default_thinking_level
                    }
                ),
                values: vec!["clear".to_string()],
                current: 0,
                payloads: vec![JsonValue::Null],
                submenu: false,
            });
        }
        let selected = active
            .and_then(|level| {
                model
                    .levels
                    .iter()
                    .position(|candidate| *candidate == level)
            })
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        Submenu {
            kind: SubmenuKind::ModelLevels { model_index },
            rows,
            selected,
        }
    }

    /// Refresh the main row's summary after an override change.
    fn update_model_thinking_summary(&mut self) {
        let overrides: Vec<String> = self
            .model_thinking_levels
            .iter()
            .map(|(model, level)| format!("{model}={level}"))
            .collect();
        let summary = if overrides.is_empty() {
            "none".to_string()
        } else if overrides.len() <= 2 {
            overrides.join(", ")
        } else {
            format!("{} overrides", overrides.len())
        };
        if let Some(row) = self
            .items
            .iter_mut()
            .find(|item| item.id == "model-thinking")
        {
            if let Some(value) = row.values.first_mut() {
                *value = summary;
            }
        }
    }
}

/// Upstream `THINKING_DESCRIPTIONS`.
fn thinking_level_description(level: &str) -> String {
    match level {
        "off" => "No extended thinking".to_string(),
        "minimal" => "Minimal reasoning budget".to_string(),
        "low" => "Low reasoning budget".to_string(),
        "medium" => "Balanced reasoning budget".to_string(),
        "high" => "High reasoning budget".to_string(),
        "xhigh" => "Extra-high reasoning budget".to_string(),
        "max" => "Maximum reasoning budget".to_string(),
        other => format!("Thinking level {other}"),
    }
}

/// The action one changed row maps to (upstream `SettingsList.onChange`
/// dispatch). Pure; unit-tested.
pub fn item_action(item: &SettingsItem) -> SettingsAction {
    let payload = item
        .payloads
        .get(item.current)
        .cloned()
        .unwrap_or(JsonValue::Null);
    if item.id == "theme" {
        // Theme rows apply live via the host; the payload write is a no-op
        // (settings_path_for("theme") is None) when the payload is malformed.
        return match payload.as_str() {
            Some(name) => SettingsAction::SetTheme(name.to_string()),
            None => SettingsAction::Set {
                id: item.id,
                value: payload,
            },
        };
    }
    SettingsAction::Set {
        id: item.id,
        value: payload,
    }
}

/// The settings.json key path one row id writes: `(key, None)` for a
/// top-level field, `(key, Some(nested))` for a key inside an object field
/// (upstream camelCase field names). `None` for rows without a direct
/// settings write.
pub fn settings_path_for(id: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match id {
        "autocompact" => ("compaction", Some("enabled")),
        "show-images" => ("terminal", Some("showImages")),
        "image-width-cells" => ("terminal", Some("imageWidthCells")),
        "auto-resize-images" => ("images", Some("autoResize")),
        "block-images" => ("images", Some("blockImages")),
        "skill-commands" => ("enableSkillCommands", None),
        "steering-mode" => ("steeringMode", None),
        "follow-up-mode" => ("followUpMode", None),
        "transport" => ("transport", None),
        "share-destination" => ("sessionShareDestination", None),
        "http-idle-timeout" => ("httpIdleTimeoutMs", None),
        "cache-warming-mode" => ("cacheWarming", None),
        "hide-thinking" => ("hideThinkingBlock", None),
        "mermaid-rendering" => ("markdown", Some("mermaid")),
        "cache-miss-notices" => ("showCacheMissNotices", None),
        "collapse-changelog" => ("collapseChangelog", None),
        "quiet-startup" => ("quietStartup", None),
        "install-telemetry" => ("enableInstallTelemetry", None),
        "default-project-trust" => ("defaultProjectTrust", None),
        "double-escape-action" => ("doubleEscapeAction", None),
        "tree-filter-mode" => ("treeFilterMode", None),
        "anthropic-extra-usage" => ("warnings", Some("anthropicExtraUsage")),
        "tui-mode" => ("tuiMode", None),
        "fullscreen-exit-output" => ("fullscreenExitOutput", None),
        "fullscreen-scrollbar" => ("fullscreenScrollbar", None),
        "fullscreen-copy-on-select" => ("fullscreenCopyOnSelect", None),
        "show-hardware-cursor" => ("showHardwareCursor", None),
        "editor-padding" => ("editorPaddingX", None),
        "output-padding" => ("outputPad", None),
        "autocomplete-max-visible" => ("autocompleteMaxVisible", None),
        "clear-on-shrink" => ("terminal", Some("clearOnShrink")),
        "terminal-progress" => ("terminal", Some("showTerminalProgress")),
        "fullscreen-wheel-scroll-lines" => ("fullscreenWheelScrollLines", None),
        _ => return None,
    })
}

/// Execute one action against the live settings store (upstream
/// `settingsManager.setXxx`): global-scope persistence. Returns the status
/// text the host shows (`None` for a no-op).
pub fn apply_settings_action(
    manager: &mut crate::config::settings::SettingsManager,
    action: &SettingsAction,
) -> Option<String> {
    match action {
        SettingsAction::Set { id, value } => {
            let (key, nested) = settings_path_for(id)?;
            // This manager instance exists for the write alone: drop its
            // load-time diagnostics so the drain below sees only the write.
            manager.drain_errors();
            match nested {
                Some(nested) => manager.set_global_nested_field(key, nested, value.clone()),
                None => manager.set_global_field(key, value.clone()),
            }
            // A refused write (corrupt settings.json) must not report
            // success; surface the refusal instead of `Saved setting`.
            let errors = manager.drain_errors();
            if let Some(error) = errors.first() {
                return Some(format!("Setting not saved: {}", error.message));
            }
            // Report the exact settings.json path the row wrote (e.g.
            // `terminal.imageWidthCells`), not the top-level key alone.
            let path = match nested {
                Some(nested) => format!("{key}.{nested}"),
                None => key.to_string(),
            };
            Some(format!("Saved setting: {path}"))
        }
        SettingsAction::SetTheme(name) => {
            // Upstream `settingsManager.setTheme` persists the setting (the
            // `"light/dark"` pair included); the host applies it live.
            manager.drain_errors();
            manager.set_global_field("theme", JsonValue::String(name.clone()));
            if let Some(error) = manager.drain_errors().first() {
                return Some(format!("Setting not saved: {}", error.message));
            }
            Some(format!("Theme: {name}"))
        }
        SettingsAction::SetModelThinking {
            provider,
            model,
            level,
        } => {
            // `modelThinkingLevels` is an object keyed `provider/id`; a clear
            // rewrites the object without the key (the manager has no
            // remove-nested API). The typed map is the effective snapshot, so
            // the whole field write keeps every other override.
            let mut levels: std::collections::BTreeMap<String, JsonValue> = manager
                .settings
                .model_thinking_levels
                .as_ref()
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|(model, level)| {
                            serde_json::to_value(level)
                                .ok()
                                .map(|value| (model.clone(), value))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let key = format!("{provider}/{model}");
            match level {
                Some(level) => {
                    levels.insert(key.clone(), JsonValue::String(level.clone()));
                }
                None => {
                    levels.remove(&key);
                }
            }
            manager.drain_errors();
            manager.set_global_field(
                "modelThinkingLevels",
                JsonValue::Object(levels.into_iter().collect()),
            );
            if let Some(error) = manager.drain_errors().first() {
                return Some(format!("Setting not saved: {}", error.message));
            }
            Some(match level {
                Some(level) => format!("Saved setting: modelThinkingLevels.{key} = {level}"),
                None => format!("Cleared setting: modelThinkingLevels.{key}"),
            })
        }
    }
}

impl Component for SettingsMenu {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold("Settings"))),
            0,
            0,
        ));
        // Upstream `SettingsList`'s search input: shown above the list while
        // the query is non-empty (type-to-search is always enabled here).
        if !state.query.is_empty() {
            frame.add_child(Text::with_padding(
                with_theme(|theme| {
                    theme.fg(ThemeColor::Muted, &format!("  Search: {}", state.query))
                }),
                0,
                0,
            ));
        }
        frame.add_child(Spacer::new(1));

        let (all_rows, visible) = state.display_rows();
        let rows: Vec<&SettingsItem> = visible
            .iter()
            .filter_map(|index| all_rows.get(*index))
            .collect();
        let selected = state
            .current_index(all_rows)
            .and_then(|current| visible.iter().position(|index| *index == current))
            .unwrap_or(0);
        // Windowed list, upstream `SettingsList` visible-window rule.
        const MAX_VISIBLE: usize = 10;
        let start = selected
            .saturating_sub(MAX_VISIBLE / 2)
            .min(rows.len().saturating_sub(MAX_VISIBLE.min(rows.len())));
        let end = (start + MAX_VISIBLE).min(rows.len());
        for (position, item) in rows.iter().enumerate().skip(start).take(end - start) {
            let cursor = if position == selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "→ "))
            } else {
                "  ".to_string()
            };
            let value_suffix = if item.submenu {
                " >".to_string()
            } else {
                format!(": {}", item.current_text())
            };
            let label = format!("{cursor}{}{value_suffix}", item.label);
            frame.add_child(Text::with_padding(
                if position == selected {
                    with_theme(|theme| theme.fg(ThemeColor::Accent, &label))
                } else {
                    label
                },
                0,
                0,
            ));
        }
        // Selected row description (upstream SettingsList description line).
        if let Some(item) = rows.get(selected) {
            frame.add_child(Spacer::new(1));
            frame.add_child(Text::with_padding(
                with_theme(|theme| theme.fg(ThemeColor::Muted, &format!("  {}", item.description))),
                0,
                0,
            ));
        }
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  ↑/↓ move · ←/→ change · enter select · type to search · esc back",
                )
            }),
            0,
            0,
        ));
        frame.add_child(DynamicBorder::new());
        frame.render(width)
    }

    fn invalidate(&self) {}
}
