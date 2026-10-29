//! Terminal UI — pi-tui-compatible component renderer.
//!
//! A faithful port of upstream `pi-tui` + `interactive-mode`: ANSI line
//! components ([`core::Text`]/[`core::BoxContainer`]/…), a differential
//! main-screen renderer ([`core::TuiCore`]), and the exact builtin
//! dark/light palettes ([`theme`]). Pure text utilities live in [`ansi`];
//! the pi `~/.pi` configuration layer (models.json + settings.json) is
//! shared with the headless hosts via `ai::providers::settings`.
//!
//! Upstream rendering contract:
//! - components produce whole lines padded to the viewport width;
//! - the diff renderer repaints only changed rows inside synchronized
//!   output (`ESC[?2026h/l`);
//! - user messages render as background blocks without any "You:" label;
//! - the editor border color tracks the thinking level / bash mode.

pub mod altscreen_search;
pub mod ansi;
pub mod app;
pub mod autocomplete;
pub mod clipboard;
pub mod command_dialogs;
pub mod core;
pub mod dialogs;
pub mod editor;
pub mod fuzzy;
pub mod image_preview;
pub mod keybindings;
pub mod markdown;
pub mod mermaid;
pub mod messages;
pub mod overlay_position;
pub mod program_status;
pub mod renderers;
pub mod selector;
pub mod session_cwd;
pub mod settings_menu;
pub mod slash;
pub mod stdin_buffer;
pub mod syntax;
pub mod terminal;
pub mod terminal_image;
pub mod terminal_input;
pub mod terminal_reply;
pub mod theme;
pub mod update_check;
pub mod wheel_scroll;

pub use ansi::{
    apply_background_to_line, pad_to_width, strip_ansi, truncate_to_width, visible_width,
    wrap_text_with_ansi,
};
// Reusable alt-screen interaction surface (§7 #22): the selection/search
// helpers a host or extension can drive without standing up the full event
// loop. These are re-exports of `altscreen_search`'s public items — no render
// code changes.
pub use altscreen_search::{
    line_selection_columns, word_selection_columns, AltScreenConfig, AltScreenState, SearchState,
    SelectionGranularity, DOUBLE_CLICK_INTERVAL,
};
pub use autocomplete::{
    builtin_commands, register_extension_command_provider, AutocompleteItem, AutocompletePopup,
    AutocompleteSuggestions, CombinedAutocompleteProvider, CommandProvider, SlashCommand,
};
pub use core::{
    BoxContainer, Component, Container, DockMouseLayout, DynamicBorder, MouseRegion, PaintedLayout,
    Rows, Spacer, Terminal, TerminalSink, Text, TuiCore,
};
pub use dialogs::{DialogBody, DialogKey, DialogOption, DialogOutcome, DialogOverlay};
pub use editor::Editor;
pub use fuzzy::{fuzzy_filter, fuzzy_match};
pub use keybindings::{Keybindings, TuiAction};
pub use renderers::{
    factory_widget_lines, mark_factory_widgets_dirty, refresh_factory_widgets,
    remove_factory_widget, renderer_registered, upsert_factory_widget, ExtensionRendererInvoker,
    FactoryRendererInvoker, FactoryWidget, RendererInvoker, RendererKind, RendererOutcome,
    RendererRequest, RENDERER_CALL_TIMEOUT, WIDGET_RENDER_REQUEST_KEY,
};
pub use selector::{SelectItem, SelectList};
pub use slash::{
    action_quits, builtin_action, copy_last_assistant_message, last_assistant_text, name_argument,
    parse_slash_command, CopyOutcome, SlashAction, SlashInvocation,
};
pub use theme::{
    all_theme_entries, current_theme_name, load_custom_themes, register_custom_theme,
    register_custom_theme_path, set_theme, set_theme_from_settings, theme_appearance,
    theme_export_colors, theme_snapshot, with_theme, Appearance, ColorMode, ExportColorsResolved,
    Theme, ThemeColor, ThemeEntry, ThemeJson, ThemeSnapshot,
};

/// The CLI flags the interactive host must apply to the session it builds.
///
/// Print mode has always honoured these; the TUI used to parse and drop them,
/// so `pir --model/--provider/--api-key/--thinking/--system-prompt/--name/
/// --models/--theme/--no-themes` had no effect in the default (interactive)
/// mode.
#[derive(Debug, Clone, Default)]
pub struct TuiOverrides {
    /// `--provider/--model/--thinking/--api-key/--models`.
    pub model: crate::coding_agent::overrides::CliModelOverrides,
    /// `--name`: the session display name.
    pub session_name: Option<String>,
    /// `--theme` entries (theme files or directories).
    pub theme_paths: Vec<std::path::PathBuf>,
    /// `--no-themes`: skip `<agent_dir>/themes` and package themes.
    pub no_themes: bool,
    /// `--system-prompt` (a literal prompt or a file path).
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` entries (text or `@file`).
    pub append_system_prompt: Vec<String>,
}

/// Entry point: run the interactive TUI (pi-compatible interactive mode).
/// `theme` is an optional explicit `--theme/--use-theme` name overriding
/// `settings.theme` (custom themes from `<agent_dir>/themes/*.json` are
/// registered first, so either name resolves). `session` is the resolved
/// [`SessionPlan`] (see `coding_agent::session_resolve`): the worker attaches
/// the planned session storage once and drives every prompt through one
/// [`crate::agent_session::AgentSession`]. `session_dir` is the configured
/// session storage directory. `resources` carries the `--skill` /
/// `--prompt-template` CLI paths and the corresponding disable flags.
/// `initial_messages` are the upstream positional
/// entry messages (`pir "hello" @file.md …`): each is enqueued to the agent
/// in order after startup (upstream `initialMessage` + `initialMessages`).
/// `tool_selection` is the host-resolved `--tools`/settings selection; it is
/// only the active override — the F12 rule: the registry is never filtered, so
/// every registered tool stays callable — and naming a `defaultActive: false`
/// tool like `powershell` is what activates it.
/// `cli_tui_mode` is the validated `--tui-mode` value (`regular`/`fullscreen`);
/// `None` falls back to `settings.tuiMode`, then the fullscreen default
/// (upstream `createInteractiveTui` options override `getTuiMode`).
/// `settings_diagnostics` are the deduplicated startup settings failures the
/// CLI collected; each one becomes a warning chat notice before the crash
/// notice (upstream `showWarning`), instead of being printed to stderr.
// Signature mirrors the upstream options object one-to-one; see the comment
// on `terminal::run` for why the lint is tolerated.
#[allow(clippy::too_many_arguments)]
pub fn run(
    extension_paths: Vec<std::path::PathBuf>,
    theme: Option<String>,
    session: crate::coding_agent::session_resolve::SessionPlan,
    session_dir: String,
    resources: crate::coding_agent::resources::ResourcePathOptions,
    initial_messages: Vec<String>,
    tool_selection: Option<crate::coding_agent::ToolSelection>,
    cli_tui_mode: Option<String>,
    settings: &crate::config::settings::SettingsManager,
    package_themes: &[std::path::PathBuf],
    settings_diagnostics: Vec<crate::config::settings::SettingsDiagnostic>,
    overrides: TuiOverrides,
) -> crate::Result<()> {
    terminal::run(
        extension_paths,
        theme,
        session,
        session_dir,
        resources,
        initial_messages,
        tool_selection,
        cli_tui_mode,
        settings,
        package_themes,
        settings_diagnostics,
        overrides,
    )
}
