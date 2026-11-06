use super::*;

/// One print pass's inputs: the shared CLI flag surface plus the per-run
/// payload (the `pir print` subcommand and the upstream `-p`/auto-print
/// entry shapes both build it).
pub(super) struct PrintModeRequest<'a> {
    pub cli: &'a Cli,
    pub config: &'a pir::config::Config,
    pub settings_manager: &'a pir::config::settings::SettingsManager,
    pub extension_paths: &'a [std::path::PathBuf],
    pub prompts: pir::coding_agent::cli::PrintPrompts,
    pub images: Vec<pir::ai::types::ImageContent>,
    pub plan: SessionPlan,
    pub json_output: bool,
}

/// Expand `@path` references in the `pir print` prompt positional.
///
/// The bare entry shapes (`pir @notes.txt`, `pir -p @notes.txt`,
/// `pir --mode json @notes.txt`) split `@` positionals in argv before they
/// reach this host. The `print` subcommand carries its prompt as ONE string,
/// so apply the same attachment pipeline
/// ([`pir::coding_agent::print::expand_print_prompt_file_args`]) to that
/// value here. `images.autoResize` (default true) selects the resizing
/// profile for supported inline images; `false` keeps the old pass-through.
/// The bare print/json paths already carry expanded `<file …>` text:
/// re-expanding them could rewrite an `@` sequence inside file content, so
/// only `Command::Print` is affected.
fn expand_print_subcommand_prompt(
    cli: &Cli,
    settings_manager: &pir::config::settings::SettingsManager,
    prompts: pir::coding_agent::cli::PrintPrompts,
    images: Vec<pir::ai::types::ImageContent>,
) -> Result<(
    pir::coding_agent::cli::PrintPrompts,
    Vec<pir::ai::types::ImageContent>,
)> {
    if !matches!(cli.command, Some(Command::Print { .. })) {
        return Ok((prompts, images));
    }
    let Some(prompt) = prompts.initial_message.clone() else {
        return Ok((prompts, images));
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (expanded, file_images) = pir::coding_agent::print::expand_print_prompt_file_args(
        &prompt,
        &cwd,
        settings_manager.settings.image_auto_resize(),
    )?;
    let mut all_images = images;
    all_images.extend(file_images);
    Ok((
        pir::coding_agent::cli::PrintPrompts {
            initial_message: Some(expanded),
            additional_messages: prompts.additional_messages,
        },
        all_images,
    ))
}

/// Run one print pass.
pub(super) async fn run_print_mode(request: PrintModeRequest<'_>) -> Result<()> {
    let PrintModeRequest {
        cli,
        config,
        settings_manager,
        extension_paths,
        prompts,
        images,
        plan,
        json_output,
    } = request;
    let (prompts, images) =
        expand_print_subcommand_prompt(cli, settings_manager, prompts, images)?;
    let outcome = pir::coding_agent::run_print(
        PrintOptions {
            prompt: prompts.initial_message.unwrap_or_default(),
            additional_messages: prompts.additional_messages,
            mode: if json_output {
                OutputMode::Json
            } else {
                OutputMode::Text
            },
            session: plan,
            no_context_files: cli.no_context_files,
            cli_provider: cli.provider.clone(),
            cli_model: cli.model.clone(),
            cli_thinking: cli.thinking_level(),
            api_key_override: cli.api_key.clone(),
            model_patterns: cli.models_patterns(),
            session_name: cli.name.clone(),
            system_prompt_override: cli.system_prompt.clone(),
            append_system_prompt: cli.append_system_prompt.clone(),
            settings_default_tools: settings_manager.default_tools(),
            cli_tools: cli.tools_entries(),
            cli_exclude_tools: cli.exclude_tools_entries(),
            no_tools: cli.no_tools,
            no_builtin_tools: cli.no_builtin_tools,
            is_continuing: cli.continue_session || cli.resume,
            resources: pir::coding_agent::resources::ResourcePathOptions {
                skill_paths: cli.skill.clone(),
                prompt_paths: cli.prompt_template.clone(),
                no_skills: cli.no_skills,
                no_prompt_templates: cli.no_prompt_templates,
            },
            images,
        },
        config,
        extension_paths,
    )
    .await?;
    if !json_output {
        // Fallible write: `println!` panics on a closed pipe (`… | head -1`),
        // which would be recorded as a crash. A broken pipe ends the process
        // quietly; any other write error is reported once.
        if let Err(error) = pir::coding_agent::output::write_stdout_line(&outcome.text) {
            pir::coding_agent::output::exit_stdout_error(error);
        }
    }
    // Upstream print-mode.ts leaves the truncation exit code to the CLI
    // (`process.exitCode = exitCode`): the partial text above is already on
    // stdout and the warning already on stderr, so exit nonzero here.
    if outcome.exit_code != 0 {
        std::process::exit(outcome.exit_code);
    }
    Ok(())
}

/// Resolve the interactive tool selection from the settings `defaultTools`
/// plus the `--tools`/`--exclude-tools`/`--no-tools`/`--no-builtin-tools`
/// flags (same shared resolution the print path applies). The selection is
/// only the worker session's ACTIVE override: it does NOT filter the worker's
/// registry (F12 — the registry stays complete, so an allowlisted
/// codemode/MCP tool stays callable and a late plugin registration is not
/// pruned), and the declared set is exactly the selected names.
///
/// This runs AFTER the CLI's strict extension validation loaded the on-disk
/// extensions (`run_mode` parks that runtime for the TUI host, which consumes
/// the same handle), and BEFORE the worker builds its `AgentSession`, whose
/// plugin host merges the builtin/JS tools into the run registry. So
/// `available_tools` must describe BOTH halves of the future merged registry:
/// - the core registry's facts (`AvailableTool::from_registry`);
/// - the builtin plugin factories' facts for this exact extension plan
///   (`builtin_plan_available_tools`, derived with the loader's own flag
///   semantics: `--no-extensions`/`-e builtin:<name>`, `--no-mcp`,
///   `pythonTool`), needed because the exclude-only and
///   `--no-builtin-tools` bases follow upstream
///   `_isActivatedOnRegistration` and cannot guess a plugin tool's
///   `exposure`/`defaultActive`;
/// - the loaded runtime's on-disk extension tools
///   (`runtime_available_tools`). Their names exist only once the runtime has
///   evaluated the files, and resolving without them dropped every name an
///   extension declared: a plain allowlist REPLACES the base, so
///   `--tools my_js_tool` matched nothing and left an EMPTY active override
///   (D10). Upstream filters the whole registry after extensions load
///   (`agent-session.ts:3525-3606`, `1595-1606`).
///
/// The three sources are sorted/deduped by name with core entries first, then
/// the builtin plan, then the on-disk tools — the same first-name-wins order
/// [`pir::agent::tool_execution::merge_plugin_tools`] applies when it merges
/// the plugin host (core registry already present, Rust builtins, then JS).
///
/// Only the interactive TUI host calls this; a build without `tui` compiles
/// it out.
#[cfg(feature = "tui")]
pub(super) fn resolve_interactive_tool_selection(
    cli: &Cli,
    settings_manager: &pir::config::settings::SettingsManager,
    extension_runtime: Option<&pir::extensions::ExtensionHandle>,
) -> Result<Option<pir::coding_agent::ToolSelection>> {
    let settings_defaults = settings_manager.default_tools();
    let cli_tools = cli.tools_entries();
    let cli_exclude = cli.exclude_tools_entries();
    // Unchanged selection: everything keeps the plugin host's exposure
    // defaults. Return before the enumeration below, which instantiates the
    // builtin plugins (the settings read codemode's config, the mcp builtin
    // reads its `mcp.json` files) — the no-flag startup must not pay that.
    // Same predicate as `resolve_tool_selection`'s unchanged-selection return.
    if !pir::coding_agent::tool_selection::tool_selection_requested(
        settings_defaults.as_deref(),
        &cli_tools,
        &cli_exclude,
        cli.no_tools,
        cli.no_builtin_tools,
    ) {
        return Ok(None);
    }

    let mut registry = pir::agent::tool_execution::ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    // Registration FACTS, not just names: the exclude-only and
    // `--no-builtin-tools` bases are `_isActivatedOnRegistration` rules.
    let mut available_tools =
        pir::coding_agent::tool_selection::AvailableTool::from_registry(&registry);
    // The same disabled-builtin composition the run's extension plan uses
    // (`disabledBuiltinExtensions`, `pythonTool: false`, `--no-mcp`), so a
    // builtin the host will not load contributes no selectable name.
    let disabled_builtins = settings_manager
        .disabled_builtins_with_flags(&pir::plugins::disabled_from_flags(cli.no_mcp));
    available_tools.extend(
        pir::coding_agent::tool_selection::builtin_plan_available_tools(
            &cli.extension,
            cli.no_extensions,
            &disabled_builtins,
        ),
    );
    // On-disk extension tools (D10). The strict validation in `run_mode`
    // already evaluated every extension file, and the TUI host consumes that
    // same parked runtime, so these are exactly the JS tools the session's
    // plugin host will merge. Without them an allowlist/exclude entry naming
    // an on-disk tool matched nothing.
    if let Some(runtime) = extension_runtime {
        available_tools.extend(pir::coding_agent::tool_selection::runtime_available_tools(
            runtime,
        ));
    }
    // Sort/dedupe by name with the core entries first (they were appended
    // first, then the builtin plan, then the runtime's JS tools), so a plugin
    // tool that shadows a core name keeps the core facts and a JS tool that
    // shadows a builtin keeps the builtin facts — the same first-name-wins
    // rule `merge_plugin_tools` applies. `sort_by` is stable, so entries with
    // equal names keep their append order before `dedup_by` drops the later
    // ones.
    available_tools.sort_by(|left, right| left.name.cmp(&right.name));
    available_tools.dedup_by(|left, right| left.name == right.name);
    pir::coding_agent::tool_selection::resolve_tool_selection_with_availability(
        settings_defaults.as_deref(),
        &cli_tools,
        &cli_exclude,
        cli.no_tools,
        cli.no_builtin_tools,
        &available_tools,
        // Base for the settings/modifier paths stays the upstream
        // `DEFAULT_TOOL_NAMES`; the exclude-only and `--no-builtin-tools`
        // paths derive their base from the registration facts above.
        &pir::config::settings::DEFAULT_TOOL_NAMES,
    )
    .map_err(anyhow::Error::msg)
}
