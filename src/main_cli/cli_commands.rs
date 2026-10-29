use super::*;

/// `--help-extensions`: the upstream `printHelp(extensionFlags)` section
/// (`cli/args.ts:273-283`) for the flags the loaded extensions registered via
/// `pi.registerFlag`.
///
/// This is a separate entry point precisely because upstream's `--help`
/// loads extensions to enumerate them: clap owns `--help` here and exits
/// before any runtime exists, so `--help` keeps the static surface (plus the
/// pointer to this command) and never executes extension code.
/// `--help-extensions` is the explicit opt-in that pays the extension load
/// (and its side effects) to print the dynamic list.
pub(super) fn run_help_extensions(
    flags: &[(pir::extensions::FlagDef, Option<String>)],
) -> Result<()> {
    println!("Extension CLI Flags:");
    if flags.is_empty() {
        println!("  (no flags registered by the loaded extensions)");
        return Ok(());
    }
    for (flag, owner) in flags {
        let value = if flag.flag_type == "string" {
            " <value>"
        } else {
            ""
        };
        let label = format!("  --{}{value}", flag.name);
        // Upstream falls back to the registering extension's path; pir's
        // runtime keeps the sanitized identity (the full path can carry a
        // user name and is never logged), which reads the same in help.
        let description = if flag.description.is_empty() {
            owner
                .as_deref()
                .map(|owner| format!("Registered by {owner}"))
                .unwrap_or_else(|| "Registered by an extension".to_string())
        } else {
            flag.description.clone()
        };
        println!("{label:<30}{description}");
    }
    Ok(())
}

/// Resolve the `--export` session argument through the existing session
/// resolution: a literal session-file path opens that file (upstream
/// `exportFromFile`), while a session id / `--session`-style reference is
/// resolved against the session dir with the same rules every host uses.
pub(super) fn resolve_export_session_path(
    reference: &str,
    config: &pir::config::Config,
) -> Result<std::path::PathBuf> {
    use pir::coding_agent::session_resolve::{resolve_session_plan, SessionFlags, SessionPlan};
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let flags = SessionFlags {
        session: Some(reference.to_owned()),
        ..SessionFlags::default()
    };
    match resolve_session_plan(
        &flags,
        &cwd,
        std::path::Path::new(&config.session_dir),
        false,
    )? {
        SessionPlan::Open(path) => Ok(path),
        // `--export` never forks, creates, or picks: a reference that does not
        // resolve to an existing session is an error (upstream reports the
        // missing file from `exportFromFile`).
        _ => Err(anyhow::anyhow!("No session found matching '{reference}'")),
    }
}

/// `--export <session> [output]`: render the resolved session to a
/// self-contained HTML file and exit (upstream `main.ts` runs `exportFromFile`
/// before any host work, prints `Exported to: <path>`, and exits 0).
pub(super) fn run_export(cli: &Cli, config: &pir::config::Config) -> Result<()> {
    let Some((reference, output)) = cli.export_request() else {
        anyhow::bail!("--export requires a session file argument");
    };
    let session_path = resolve_export_session_path(&reference, config)?;
    let output_path = output.map(std::path::PathBuf::from);
    match pir::coding_agent::export_html::export_session_to_html(
        &session_path,
        output_path.as_deref(),
    ) {
        Ok(written) => {
            println!("Exported to: {}", written.display());
            Ok(())
        }
        Err(error) => Err(anyhow::anyhow!("{error}")),
    }
}

/// Human-readable token count (upstream list-models.ts `formatTokenCount`:
/// `200000` → `"200K"`, `1000000` → `"1M"`, non-integral → one decimal).
pub(super) fn format_token_count(count: u64) -> String {
    if count >= 1_000_000 {
        let millions = count as f64 / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{}M", millions as u64)
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = count as f64 / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{}K", thousands as u64)
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

/// Print the available models with an optional fuzzy search filter (upstream
/// `cli/list-models.ts`): a padded provider/model/context/max-out/thinking/
/// images table, fuzzy-filtered on `"<provider> <id>"`.
pub(super) fn run_list_models(
    search: Option<&str>,
    settings: &pir::config::settings::SettingsManager,
) -> Result<()> {
    // The fuzzy matcher lives in the TUI module (its scoring is tuned for the
    // TUI pickers); a build without `tui` falls back to a plain
    // case-insensitive substring filter below.
    #[cfg(feature = "tui")]
    use pir::tui::fuzzy::fuzzy_filter;

    // Reuse the caller's settings snapshot (the metadata fast path already
    // resolved trust and loaded both scopes once) instead of re-reading and
    // re-parsing `settings.json` here.
    let environment = pir::ai::providers::load_pi_environment_with_settings(settings)?;
    let mut models = environment.models.clone();
    if models.is_empty() {
        // Upstream `formatNoModelsAvailableMessage`.
        println!(
            "No models available. Use /login to log into a provider via OAuth or API key. See:"
        );
        println!("  providers.md");
        println!("  models.md");
        return Ok(());
    }
    if let Some(search) = search.map(str::trim).filter(|search| !search.is_empty()) {
        // Upstream fuzzy-filters on `"<provider> <id>"` (list-models.ts:49).
        #[cfg(feature = "tui")]
        {
            // fuzzy_filter's text accessor borrows the item, so pair each
            // model with its haystack first.
            let entries: Vec<(String, _)> = models
                .into_iter()
                .map(|model| (format!("{} {}", model.provider, model.id), model))
                .collect();
            models = fuzzy_filter(&entries, search, |(haystack, _model)| haystack)
                .into_iter()
                .map(|(_haystack, model)| model)
                .collect();
        }
        #[cfg(not(feature = "tui"))]
        {
            // Headless profile: same haystack, case-insensitive substring
            // match (the TUI fuzzy scorer is compiled out).
            let needle = search.to_lowercase();
            models.retain(|model| {
                format!("{} {}", model.provider, model.id)
                    .to_lowercase()
                    .contains(&needle)
            });
        }
        if models.is_empty() {
            println!("No models matching \"{search}\"");
            return Ok(());
        }
    }
    // Sort by provider, then by model id (upstream `localeCompare` order).
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.id.cmp(&b.id)));

    struct Row {
        provider: String,
        model: String,
        context: String,
        max_out: String,
        thinking: &'static str,
        images: &'static str,
    }
    let rows: Vec<Row> = models
        .iter()
        .map(|model| Row {
            provider: model.provider.clone(),
            model: model.id.clone(),
            context: format_token_count(model.context_window),
            max_out: format_token_count(model.max_tokens),
            thinking: if model.reasoning { "yes" } else { "no" },
            images: if model.input.iter().any(|input| input == "image") {
                "yes"
            } else {
                "no"
            },
        })
        .collect();

    // Column widths are the max of the header and every row (upstream padEnd).
    let widths = [
        rows.iter()
            .map(|r| r.provider.len())
            .chain(["provider".len()])
            .max()
            .unwrap_or(0),
        rows.iter()
            .map(|r| r.model.len())
            .chain(["model".len()])
            .max()
            .unwrap_or(0),
        rows.iter()
            .map(|r| r.context.len())
            .chain(["context".len()])
            .max()
            .unwrap_or(0),
        rows.iter()
            .map(|r| r.max_out.len())
            .chain(["max-out".len()])
            .max()
            .unwrap_or(0),
        rows.iter()
            .map(|r| r.thinking.len())
            .chain(["thinking".len()])
            .max()
            .unwrap_or(0),
        rows.iter()
            .map(|r| r.images.len())
            .chain(["images".len()])
            .max()
            .unwrap_or(0),
    ];
    let line = |cells: [&str; 6]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!(
        "{}",
        line(["provider", "model", "context", "max-out", "thinking", "images"])
    );
    for row in &rows {
        println!(
            "{}",
            line([
                &row.provider,
                &row.model,
                &row.context,
                &row.max_out,
                row.thinking,
                row.images
            ])
        );
    }
    Ok(())
}
