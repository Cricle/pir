//! First-run setup (upstream `cli/startup-ui.ts` + `modes/interactive/
//! components/first-time-setup.ts`): the experimental first-launch dialog that
//! asks for a theme and the anonymous-analytics opt-in before any runtime
//! service is created, then persists both to the global `settings.json`.
//!
//! Upstream gates the dialog on all of:
//! - the official distribution (constant here: the `pir` binary is its own
//!   official distribution, so the check in `shouldRunFirstTimeSetup` always
//!   holds),
//! - experimental features enabled (`PI_EXPERIMENTAL=1`,
//!   `core/experimental.ts`),
//! - the default agent directory (no `PI_CODING_AGENT_DIR` override; pir also
//!   accepts its `PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` aliases),
//! - no global `settings.json` yet (the marker that setup was not completed).
//!
//! The caller adds the invocation half (upstream `main.ts:675-680`): the
//! interactive TTY app mode only, and never for `--help`/`--list-models`
//! (pir also skips `--export`, which upstream handles before this point).
//!
//! Unlike upstream's throwaway full-screen TUI, pir's prompt is
//! line-oriented: the subcommand path runs before the interactive host owns
//! the terminal, and a throwaway TUI would need the whole session stack. The
//! choices, their order and the preselected defaults are upstream's
//! (`THEME_OPTIONS` / `ANALYTICS_OPTIONS`): theme `system` and "share
//! anonymous usage data". Enter accepts the preselected option; EOF or an
//! out-of-range choice cancels (nothing is written, so the next interactive
//! launch asks again — upstream's cancel path leaves the settings absent too).

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::Value;

use crate::config::settings::SettingsManager;

/// Theme choices in upstream order (`THEME_OPTIONS`, first-time-setup.ts:19-23).
pub const THEME_OPTIONS: [(&str, &str); 3] = [
    ("system", "System (matches your terminal colors)"),
    ("dark", "Dark"),
    ("light", "Light"),
];

/// Analytics choices in upstream order (`ANALYTICS_OPTIONS`,
/// first-time-setup.ts:25-28). The first entry is preselected, matching
/// upstream's `analyticsIndex = 0` ("Share anonymous usage data").
pub const ANALYTICS_OPTIONS: [(bool, &str); 2] =
    [(true, "Share anonymous usage data"), (false, "Don't share")];

/// The dialog's result (upstream `FirstTimeSetupResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstTimeSetupResult {
    /// The chosen theme name (`system` / `dark` / `light`).
    pub theme: String,
    /// The analytics opt-in choice.
    pub share_analytics: bool,
}

/// Upstream `shouldRunFirstTimeSetup` (startup-ui.ts:131-148): experimental
/// features, the default agent directory, and no global settings file. The
/// distribution check is constant for pir and lives in the module docs.
pub fn should_run_first_time_setup(
    settings_path: &Path,
    experimental: bool,
    custom_agent_dir: bool,
) -> bool {
    experimental && !custom_agent_dir && !settings_path.exists()
}

/// `PI_EXPERIMENTAL=1` (upstream `areExperimentalFeaturesEnabled`).
pub fn experimental_enabled() -> bool {
    std::env::var("PI_EXPERIMENTAL").is_ok_and(|value| value == "1")
}

/// Whether an agent-dir environment override is in play (upstream
/// `process.env[ENV_AGENT_DIR]`; pir's agent-dir resolution also accepts its
/// own aliases — see [`crate::ai::providers::agent_dir`]).
pub fn custom_agent_dir() -> bool {
    [
        "PIR_AGENT_DIR",
        "PIR_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The invocation half of the gate (upstream `main.ts:675-680`): interactive
/// mode only, and never for a metadata command (`--help`/`--list-models`;
/// `--export` is pir's extra metadata command).
pub fn should_run_for_invocation(interactive: bool, metadata_command: bool) -> bool {
    interactive && !metadata_command
}

/// Show the first-run prompt on `reader`/`writer` and return the selection.
///
/// `Ok(None)` when the input ended or the choice was out of range: the caller
/// then leaves `settings.json` untouched (the upstream cancel path).
pub fn prompt_first_time_setup<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<Option<FirstTimeSetupResult>> {
    writeln!(writer, "Welcome to pir, the minimal coding agent.")?;
    writeln!(writer)?;
    writeln!(writer, "Pick a theme.")?;
    for (index, (_value, label)) in THEME_OPTIONS.iter().enumerate() {
        writeln!(writer, "  {}. {label}", index + 1)?;
    }
    write!(writer, "Theme [1]: ")?;
    writer.flush()?;
    let Some(theme_index) = read_choice(reader, THEME_OPTIONS.len())? else {
        return Ok(None);
    };

    writeln!(writer)?;
    writeln!(writer, "Opt-in to anonymous usage data sharing?")?;
    writeln!(
        writer,
        "Opting in stores a tracking identifier in settings.json and enables anonymous\n\
         usage analytics. This helps us to better debug, reproduce, and resolve issues\n\
         and bugs within pir. You can observe what is shared using /privacy and make\n\
         changes anytime in settings.json."
    )?;
    for (index, (_value, label)) in ANALYTICS_OPTIONS.iter().enumerate() {
        writeln!(writer, "  {}. {label}", index + 1)?;
    }
    write!(writer, "Analytics [1]: ")?;
    writer.flush()?;
    let Some(analytics_index) = read_choice(reader, ANALYTICS_OPTIONS.len())? else {
        return Ok(None);
    };

    Ok(Some(FirstTimeSetupResult {
        theme: THEME_OPTIONS[theme_index].0.to_string(),
        share_analytics: ANALYTICS_OPTIONS[analytics_index].0,
    }))
}

/// Read one choice line: empty accepts the first option (upstream's selected
/// index 0 + Enter), a valid 1-based index selects it, anything else (or EOF)
/// cancels.
fn read_choice<R: BufRead>(reader: &mut R, count: usize) -> std::io::Result<Option<usize>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(Some(0));
    }
    match trimmed.parse::<usize>() {
        Ok(picked) if (1..=count).contains(&picked) => Ok(Some(picked - 1)),
        _ => Ok(None),
    }
}

/// Persist the dialog's result (upstream `settingsManager.setTheme` +
/// `setEnableAnalytics` + `flush`): the global scope only, so a project
/// settings file can never carry a per-user choice. Returns the recorded
/// write errors (empty on success); the caller reports them as warnings.
///
/// All fields — theme, analytics opt-in, the generated tracking id, and an
/// explicit `firstRunSetupCompleted` marker — are committed in ONE atomic
/// write. The startup gate treats "settings.json exists" as "setup already
/// ran", so a crash or failed write between two independent writes used to
/// leave a file that suppressed the prompt while missing half the answer.
/// With a single temp-file replace, the marker and the values it certifies
/// always land together (or the file stays absent and the next launch asks
/// again).
pub fn apply_first_time_setup(
    settings: &mut SettingsManager,
    result: &FirstTimeSetupResult,
) -> Vec<String> {
    let mut fields: Vec<(&str, Value)> = vec![
        ("theme", Value::String(result.theme.clone())),
        ("enableAnalytics", Value::Bool(result.share_analytics)),
        ("firstRunSetupCompleted", Value::Bool(true)),
    ];
    if result.share_analytics && settings.tracking_id().is_none() {
        fields.push((
            "trackingId",
            Value::String(uuid::Uuid::new_v4().to_string()),
        ));
    }
    settings.set_global_fields(&fields);
    settings
        .drain_errors()
        .into_iter()
        .map(|error| format!("{} settings: {}", error.scope_name(), error.message))
        .collect()
}
