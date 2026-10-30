use super::*;

pub(super) async fn run() -> Result<()> {
    // Upstream `cli/setup.ts:6-7` / `rpc-entry.ts:7-8`: extensions, hooks, and
    // child tools detect that they run under the coding agent. Set before any
    // resource or extension load so first-turn scripts observe it too.
    std::env::set_var("PI_CODING_AGENT", "true");
    std::env::set_var("AI_AGENT", "pi");
    // This process is the `pir` binary: the shell tools may spawn it as the
    // embedded shell (`__brush`). Library hosts never set this, so a test or
    // SDK embedder keeps spawning the external shell instead of handing
    // `__brush` to a program that does not understand it.
    #[cfg(feature = "brush")]
    pir::shell_embed::mark_host_binary();
    // Upstream-style argv preprocessing: bundled short flags, unknown
    // `--flag[=value]` collection for extensions. `args_os` + lossy
    // conversion: `env::args` panics on non-UTF-8 argv.
    let mut benchmark = pir::config::timings::StartupTimings::new();
    let raw_args: Vec<String> = std::env::args_os().skip(1).map(lossy_arg).collect();
    // `pir js <file> [args...]`: the subprocess script runner (node-compat
    // roadmap item B). Intercepted BEFORE argv preprocessing — script
    // arguments are verbatim `process.argv` material, so an unknown
    // `--flag=value` must reach the script rather than the extension-flag
    // collector (the same reason the hidden codemode worker intercepts
    // early). No settings/trust/migrations/tracing setup and no extension
    // loading: a script run owns the process stdio and only loads its own
    // entry file (see `js_mode.rs`).
    if raw_args.first().map(String::as_str) == Some("js") {
        #[cfg(feature = "extensions")]
        {
            let code = crate::js_mode::run(&raw_args[1..]);
            std::process::exit(code);
        }
        #[cfg(not(feature = "extensions"))]
        {
            eprintln!(
                "Error: `pir js` is unavailable: this build was built without the `extensions` feature"
            );
            std::process::exit(1);
        }
    }
    // `pir env-daemon serve --token <hex>`: the built-in pi-env protocol-v1
    // daemon (upstream `packages/env/daemon`, argv shape preserved). The env
    // SDK shim spawns `<current_exe> env-daemon …` and speaks the framed v1
    // protocol over stdio; upstream pi-env clients can deploy pir as their
    // daemon the same way. Intercepted on the RAW argv — like `js`, before
    // argv preprocessing — so `--token <hex>` is never swallowed by the
    // unknown-flag collector, and before clap so the subcommand stays out of
    // `--help`. See `pir::env_daemon`.
    if raw_args.first().map(String::as_str) == Some(pir::env_daemon::DAEMON_SUBCOMMAND) {
        #[cfg(feature = "env-daemon")]
        {
            std::process::exit(pir::env_daemon::run_main(&raw_args[1..]));
        }
        // The daemon half is a feature, but the subcommand is intercepted
        // before clap in every profile: report the documented missing-feature
        // error instead of an unrecognized subcommand (the `pir js` pattern).
        #[cfg(not(feature = "env-daemon"))]
        {
            eprintln!(
                "Error: `pir env-daemon` is unavailable: this build was built without the `env-daemon` feature"
            );
            std::process::exit(1);
        }
    }
    // `pir mcp-sd [--root <dir>]`: the built-in `sd` (search & replace) MCP
    // server hosted by this same executable (single-binary policy; rmcp's
    // server half). Intercepted on the RAW argv like `js`/`env-daemon`: the
    // MCP client that spawns it owns stdout, so the process must write
    // nothing there but JSON-RPC frames, and starting before argv
    // preprocessing/clap keeps the subcommand out of `--help` and keeps
    // settings, trust, migrations, extension loading and tracing setup from
    // touching this process's stdio. See `pir::mcp::server`.
    #[cfg(feature = "mcp-sd")]
    if raw_args.first().map(String::as_str) == Some(pir::mcp::server::SUBCOMMAND) {
        std::process::exit(pir::mcp::server::run_main(&raw_args[1..]).await);
    }
    // `mcp` is on but the built-in server is not: `pir::mcp::server` is gated
    // on `mcp-sd`, so the arm above is not compiled here. Keep the exact
    // missing-feature diagnostic instead of a clap "unrecognized subcommand".
    #[cfg(all(feature = "mcp", not(feature = "mcp-sd")))]
    if raw_args.first().map(String::as_str) == Some("mcp-sd") {
        eprintln!(
            "Error: `pir mcp-sd` is unavailable: this build was built without the `mcp-sd` feature"
        );
        std::process::exit(1);
    }
    // No MCP client either: the subcommand is intercepted before clap in every
    // profile, so report the missing feature rather than letting an
    // unrecognized subcommand error out on argv that is valid in `full`.
    #[cfg(not(feature = "mcp"))]
    if raw_args.first().map(String::as_str) == Some("mcp-sd") {
        eprintln!(
            "Error: `pir mcp-sd` is unavailable: this build was built without the `mcp` feature"
        );
        std::process::exit(1);
    }
    // `pir __brush [-c <script>]`: the embedded brush shell the shell pool
    // spawns instead of Git Bash. Intercepted on the RAW argv (like
    // `__codemode-worker`): it is an internal child-process entry, and the
    // pooled member owns its stdio, so nothing may run before it.
    #[cfg(feature = "brush")]
    if raw_args.first().map(String::as_str) == Some("__brush") {
        let code = match pir::shell_embed::run_brush_main(&raw_args[1..]) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("pir __brush: {error}");
                1
            }
        };
        std::process::exit(code);
    }
    #[cfg(not(feature = "brush"))]
    if raw_args.first().map(String::as_str) == Some("__brush") {
        return Err(missing_feature("pir __brush", "brush"));
    }
    // Native `grep`/`sed`/`python`: standalone commands that own their argv
    // and stdio (see `native_cli`). Intercept them on the RAW argv — like
    // `js`/`env-daemon` — so a grep invocation neither parses the 60+ flag
    // CLI surface nor runs migrations/settings/trust/package discovery before
    // doing its one job. The clap arms below still serve the
    // global-flags-before-subcommand spelling (`pir --verbose grep …`).
    if let Some(command) = raw_args.first().map(String::as_str) {
        let native = match command {
            "grep" => Some(pir::native_cli::run_grep as fn(&[String]) -> pir::Result<()>),
            "sed" => Some(pir::native_cli::run_sed as fn(&[String]) -> pir::Result<()>),
            "python" => Some(pir::native_cli::run_python as fn(&[String]) -> pir::Result<()>),
            _ => None,
        };
        if let Some(run) = native {
            if let Err(error) = run(&raw_args[1..]) {
                eprintln!("pir {command}: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
    let (mut args, unknown_flags) = preprocess_args(raw_args);
    // clap's parse_from expects argv[0] first.
    args.insert(
        0,
        std::env::args_os()
            .next()
            .map(lossy_arg)
            .unwrap_or_else(|| "pir".to_string()),
    );
    // Hidden codemode worker entry (upstream worker-process isolation): the
    // codemode plugin spawns `<current_exe> __codemode-worker` and drives a
    // versioned JSON-lines sandbox protocol over stdio. Intercepted BEFORE
    // clap parsing so the subcommand stays out of `--help` (and never
    // collides with the public subcommands); see
    // `pir::plugins::codemode::worker`.
    #[cfg(feature = "extensions")]
    if args.len() == 2 && args[1] == pir::plugins::codemode::worker::WORKER_SUBCOMMAND {
        return pir::plugins::codemode::worker::run_worker_main();
    }
    // The worker is an internal child-process entry only the codemode plugin
    // spawns; without `extensions` there is no plugin to spawn it. Keep the
    // name out of clap's error path with the same explicit missing-feature
    // message the other compiled-out entry points use.
    #[cfg(not(feature = "extensions"))]
    if args.len() == 2 && args[1] == "__codemode-worker" {
        return Err(missing_feature("pir __codemode-worker", "extensions"));
    }
    // Parse with explicit exit-code control (upstream main.ts): `--help`
    // exits 0, parse errors exit 1 (upstream prints `Unknown option: …` and
    // exits 1), and `-v`/`--version` prints ONLY the semver (main.ts:634-637).
    let mut cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            match error.kind() {
                ErrorKind::DisplayVersion => {
                    println!("{}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                ErrorKind::DisplayHelp => {
                    let _ = error.print();
                    std::process::exit(0);
                }
                _ => {
                    let _ = error.print();
                    std::process::exit(1);
                }
            }
        }
    };
    cli.unknown_flags = unknown_flags;

    // Upstream main.ts post-parse validation: every failure prints
    // `Error: …` and exits 1 before any mode work starts. (The provider rule
    // internally exempts `auth` subcommands, which own their --provider.)
    if let Err(message) = pir::coding_agent::cli::validate_parsed_flags(&cli) {
        eprintln!("Error: {message}");
        std::process::exit(1);
    }
    benchmark.time("parseArgs");

    // `--offline` disables startup network operations process-wide
    // (PI_OFFLINE; version-check consumers honor PI_SKIP_VERSION_CHECK too).
    if cli.offline {
        std::env::set_var("PI_OFFLINE", "1");
    }
    // `--verbose` forces verbose startup tracing, and overrides the
    // `quietStartup` startup-output gates (upstream `options.verbose`).
    if cli.verbose {
        if std::env::var_os("RUST_LOG").is_none() {
            std::env::set_var("RUST_LOG", "debug");
        }
        std::env::set_var("PIR_VERBOSE", "1");
    }

    // F16: when the interactive TUI is about to run, tracing must not write
    // stderr — the TUI owns the terminal surface and stray records
    // (--verbose debug output, catalog-refresh warnings) corrupt the
    // fullscreen/regular frames. Route tracing into `<agentDir>/pir.log`
    // (size-capped at startup, one rotated generation). json/rpc/print modes
    // keep stderr: json/rpc speak the protocol over stdio and a stray record
    // on stdout would corrupt the stream; print surfaces errors on stderr.
    let app_mode_tui = {
        let stdin_tty = std::io::stdin().is_terminal();
        let stdout_tty = std::io::stdout().is_terminal();
        cli.entry_plan(stdin_tty, stdout_tty).app_mode
            == pir::coding_agent::cli::AppMode::Interactive
    };
    // `rustpython_vm=warn`: the embedded interpreter logs every caught
    // exception at INFO (`Exception raised: PyBaseException`) — normal
    // control flow inside the frozen stdlib, and pure noise on the CLI's
    // stderr. Warnings and errors still surface.
    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rustpython_vm=warn"));
    if app_mode_tui {
        let log_path = pir::coding_agent::context::agent_dir().join("pir.log");
        rotate_tui_trace_log(&log_path, TUI_TRACE_LOG_MAX_BYTES);
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(log_file) => {
                tracing_subscriber::fmt()
                    .with_env_filter(log_filter)
                    .with_writer(std::sync::Arc::new(log_file))
                    .init();
            }
            Err(error) => {
                // No usable log file (read-only agent dir?): stderr is the
                // lesser evil over losing tracing entirely.
                eprintln!(
                    "pir: tracing to stderr (cannot open log {}: {error})",
                    log_path.display()
                );
                tracing_subscriber::fmt()
                    .with_env_filter(log_filter)
                    .with_writer(std::io::stderr)
                    .init();
            }
        }
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(log_filter)
            .with_writer(std::io::stderr)
            .init();
    }

    // `--list-models [search]` metadata fast path. Model metadata lives in
    // `settings.json`/`models.json`/the built-in catalog; nothing below this
    // point is needed to print it. In particular the normal startup path would
    // resolve the extension plan (which scans settings/discovery directories
    // and can spawn `npm install`/`git clone` for configured packages), run
    // the trust-extension bootstrap, validate every extension, and install
    // packages before reaching the `--list-models` arm. Metadata commands
    // must never pay those costs. Trust is still resolved through the
    // metadata policy (stored/default decision, never a selector) so a
    // trusted project's `models.json` provider settings stay visible, and
    // the settings diagnostics the command reports on stderr are preserved.
    //
    // `--help-extensions` inherently loads extensions (it prints what they
    // registered) and `--export` needs the resolved session directory, so
    // both keep the regular path.
    if cli.list_models.is_some() && !cli.help_extensions && cli.export.is_none() {
        let agent_dir = pir::coding_agent::context::agent_dir();
        let project_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        // pi-compatible agent-dir bootstrap (upstream `core/auth-storage.ts`
        // `ensureFileExists` + `core/models-store.ts`): a normal startup leaves
        // `auth.json` and `models-store.json` as `{}`. Upstream runs the
        // legacy-credential migration BEFORE the credential store creates
        // `auth.json` (main.ts `runMigrations` before `createRuntime`), so the
        // metadata fast path migrates `oauth.json`/settings `apiKeys` first:
        // a fresh empty `auth.json` would otherwise suppress that migration.
        pir::config::migrations::migrate_auth_to_auth_json(&agent_dir);
        pir::coding_agent::agent_bootstrap::bootstrap_agent_dir(&agent_dir);
        let project_trusted = cli.trust_override().unwrap_or_else(|| {
            pir::extensions::project_trust::resolve_project_trust(&project_dir, &agent_dir)
        });
        let mut settings = SettingsManager::load(
            &agent_dir,
            &project_dir.join(".pi"),
            &project_dir,
            project_trusted,
        );
        for diagnostic in pir::config::settings::deduplicate_diagnostics(
            pir::config::settings::collect_settings_diagnostics(&mut settings),
        ) {
            eprintln!("{}", diagnostic.stderr_line());
        }
        let search = cli
            .list_models
            .as_deref()
            .filter(|search| !search.is_empty());
        return run_list_models(search, &settings);
    }

    let mut config = pir::config::Config::from_env();

    // `--no-context-files` disables discovery process-wide (the shared
    // facade reads the same switch).
    if cli.no_context_files {
        std::env::set_var(pir::coding_agent::context::NO_CONTEXT_FILES_ENV, "1");
    }

    let mode = cli.mode();
    let agent_dir = pir::coding_agent::context::agent_dir();
    let project_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // One-time startup migrations (upstream main.ts:668 `runMigrations(cwd)`):
    // legacy auth storage → `auth.json`, stray agent-root session files into
    // the per-cwd session directories, managed binaries `tools/` → `bin/`,
    // and `commands/` → `prompts/` plus deprecation-warning collection.
    // pir's `keybindings.json` migration stays in its loader
    // (src/config/keybindings.rs) — not duplicated here.
    // The collected deprecation warnings are shown by the interactive hosts
    // only; a build without `tui` runs the migrations for their side effects
    // and never reads the result.
    #[cfg_attr(not(feature = "tui"), allow(unused_variables))]
    let migrations = pir::config::migrations::run_migrations(&project_dir);
    benchmark.time("runMigrations");

    // pi-compatible agent-dir bootstrap (upstream `core/auth-storage.ts`
    // `ensureFileExists` + `core/models-store.ts`): a normal startup leaves
    // `<agentDir>/auth.json` and `<agentDir>/models-store.json` as `{}`.
    // Runs AFTER migrations for the same reason upstream does: a fresh
    // `auth.json` would otherwise suppress the legacy-credential migration.
    if bootstraps_agent_dir(&cli) {
        pir::coding_agent::agent_bootstrap::bootstrap_agent_dir(&agent_dir);
    }

    // Fatal-panic persistence (upstream `core/crash-log.ts`): every panic —
    // including the TUI host's, whose hook chains into this one — appends a
    // bounded record to `<agentDir>/crashes.json` before the panic message
    // prints. The hook re-resolves the agent dir at panic time, matching the
    // crash notice's own live resolution (`take_unnotified_crash`), so the
    // two can never name different directories.
    pir::coding_agent::crash_log::install_hook();

    // Upstream entry contract (main.ts `resolveAppMode` + `parseArgs`):
    // resolve the app mode from the TTY state and the `-p/--print` flag, and
    // split the positional messages from `@file` arguments. `@file` + RPC is
    // a hard error (upstream main.ts:659).
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let entry = cli.entry_plan(stdin_tty, stdout_tty);
    if let Err(message) =
        pir::coding_agent::cli::validate_rpc_file_args(entry.app_mode, &entry.file_args)
    {
        eprintln!("Error: {message}");
        std::process::exit(1);
    }

    // Project trust (upstream `resolveProjectTrusted`) decides whether the
    // `.pi` settings/resources of this checkout load at all. Trust resolves
    // with the pre-stdin app mode (upstream runs the trust context before
    // `readPipedStdin`).
    let interactive = match &cli.command {
        Some(Command::Interactive) => entry.app_mode != AppMode::Rpc,
        Some(_) => false,
        None => entry.app_mode == AppMode::Interactive,
    };
    // First-run setup (upstream main.ts:675-680): the experimental
    // theme/analytics dialog runs before the trust decision and before the
    // real settings load, so the chosen values apply to this same run. The
    // gate needs a real interactive TTY (`entry.app_mode`, not `interactive`:
    // `pir interactive` with piped stdin must not block on a prompt) and never
    // fires for a metadata command (`--list-models`/`--export`/
    // `--help-extensions`).
    if entry.app_mode == AppMode::Interactive
        && matches!(cli.command, None | Some(Command::Interactive))
        && !cli.is_metadata_command()
    {
        run_first_time_setup_if_needed(&agent_dir, &project_dir);
    }
    // Metadata commands (`--list-models`/`--export`/`--help-extensions`)
    // resolve trust through the stored/default decision even on a TTY:
    // upstream `trustPromptMode` is "print" for them, and a selector must
    // never appear before their output.
    //
    // Extensions are the FIRST trust input after the explicit CLI override
    // (upstream `resolveProjectTrusted`, project-trust.ts:47-71): run the
    // pre-trust pass over the user/global/CLI plan so a `project_trust`
    // handler can grant or deny the project before the store/policy decides.
    // This mirrors `ResourceLoader.loadProjectTrustExtensions`: project
    // settings and project extension paths stay out of the bootstrap plan,
    // and the runtime is disposable.
    // Only the mode hosts that actually load extensions resolve the plan
    // (print/interactive/rpc, plus the no-subcommand entry). Subcommands —
    // including `--export`, whose upstream path runs before extension
    // loading — must not scan settings/discovery directories or install
    // configured packages on the way to their own work.
    let host_loads_extensions = matches!(
        cli.command,
        None | Some(Command::Interactive) | Some(Command::Print { .. }) | Some(Command::Rpc)
    ) && cli.export.is_none();
    let rpc_command = mode == Some(Mode::Rpc) || matches!(cli.command, Some(Command::Rpc));
    let mut bootstrap_settings: Option<SettingsManager> = None;
    if host_loads_extensions
        && cli.trust_override().is_none()
        && pir::extensions::project_trust::env_override_decision().is_none()
        && pir::config::trust::has_trust_requiring_project_resources(&project_dir)
    {
        let settings =
            SettingsManager::load(&agent_dir, &project_dir.join(".pi"), &project_dir, false);
        let config =
            super::cli_preprocess::extension_runtime_config(rpc_command, interactive, &project_dir);
        super::cli_preprocess::bootstrap_project_trust_extensions(&cli, &settings, config).await;
        // Reused below when the extension pass leaves the project untrusted:
        // it is exactly the manager the final trust state asks for.
        bootstrap_settings = Some(settings);
    }
    let project_trusted = resolve_project_trust_for_run(
        &cli,
        &agent_dir,
        cli.trust_prompt_has_ui(interactive) && stdin_tty && stdout_tty,
    );
    // Freeze this run's decision for every later consumer in the process:
    // `--approve`/`--no-approve` and the startup selector's session-only rows
    // have no other carrier, and the final extension load (which includes
    // project extensions once trust is granted) must not overwrite the
    // decision the gate used.
    pir::extensions::project_trust::pin_decision(&project_dir, project_trusted);

    let mut settings_manager = if project_trusted {
        // Trust granted between the bootstrap and the final read (an
        // extension decision or the selector): reload so the project scope
        // takes effect for this run.
        SettingsManager::load(&agent_dir, &project_dir.join(".pi"), &project_dir, true)
    } else {
        bootstrap_settings.unwrap_or_else(|| {
            SettingsManager::load(&agent_dir, &project_dir.join(".pi"), &project_dir, false)
        })
    };
    benchmark.time("loadSettings");
    // Upstream `collectSettingsDiagnostics` + `deduplicateDiagnostics`
    // (core/settings-diagnostics.ts, main.ts): every recorded settings load
    // failure becomes a warning diagnostic naming the file, and duplicates
    // (the startup and runtime managers can report the same file) collapse to
    // their first occurrence. Non-interactive paths print
    // `Warning: <message>` to stderr; the interactive TUI receives the same
    // list and shows one warning notice per diagnostic instead (upstream
    // interactive `showWarning`), so the stderr lines are not duplicated under
    // the alternate screen.
    let startup_settings_diagnostics = pir::config::settings::deduplicate_diagnostics(
        pir::config::settings::collect_settings_diagnostics(&mut settings_manager),
    );
    if !interactive {
        for diagnostic in &startup_settings_diagnostics {
            eprintln!("{}", diagnostic.stderr_line());
        }
    }
    config.session_dir = resolve_session_dir(&cli, &settings_manager, &config.session_dir);

    // `--export <session> [output]`: render the session to a self-contained
    // HTML page and exit before any host/model/extension work (upstream
    // `main.ts` handles `parsed.export` before starting a mode). This runs
    // before the tool-selection snapshot because it needs nothing from it.
    if cli.export.is_some() {
        return run_export(&cli, &config);
    }

    // The global tool-selection inputs, snapshotted ONCE for every host that
    // resolves them later. RPC resolves against its loaded core + builtin +
    // JS registry; the server resolves against its policy-filtered core
    // registry (upstream has no server host, so this is pir's consistency
    // rule: the same CLI/settings contract as print and the TUI).
    let tool_selection_request = cli.tool_selection_request(&settings_manager);

    // `--tui-mode` is consumed by the interactive host; validate early so a
    // typo errors before any work happens.
    validate_tui_mode(&cli)?;

    // Only hosts that actually load the extension runtime resolve the plan;
    // every subcommand keeps the default (empty) plan, so no settings,
    // discovery or package scan runs on the way to its own work.
    let extension_plan = if host_loads_extensions {
        resolve_extension_paths(&cli, &settings_manager, project_trusted)
    } else {
        super::cli_preprocess::ResolvedExtensionPaths::default()
    };
    let extension_paths = extension_plan.paths;
    let default_disabled_skips = extension_plan.default_disabled_skips;
    let only_builtin_ignored = extension_plan.only_builtin_ignored;
    // Package-contributed resources (settings.packages): the SAME scan the
    // extension plan just performed — the strict gate classifies package
    // extension failures against its `extensions`, and the TUI's theme load
    // consumes its `themes` without a second walk of the package tree.
    let package_resources = extension_plan.package_resources;
    // No JS engine in this build: on-disk extensions cannot load, and keeping
    // them on the plan would make the strict validation below fail startup
    // with the runtime's "no JS extension runtime" error. Drop them with one
    // diagnostic instead; `builtin:<name>` entries are Rust plugins and stay
    // (a builtin-only plan runs on the plan-only handle).
    #[cfg(not(feature = "extensions"))]
    let extension_paths = {
        let (builtins, ignored) = pir::plugins::filter_builtin_paths(&extension_paths);
        if !ignored.is_empty() {
            eprintln!(
                "pir: this build has no JS extension runtime (the `extensions` feature is off); \
                 ignoring {} on-disk extension path(s)",
                ignored.len()
            );
        }
        builtins
    };

    // Upstream `loadExtensionPaths` failure semantics: an extension that
    // fails to load is a hard error with a `-ne` hint, before any mode starts.
    // The validating load is not thrown away: the runtime that proved the set
    // loads is parked for the mode host and consumed there (single discovery,
    // transpile and evaluation — upstream shares one runtime across its
    // pre-trust and final passes, resource-loader.ts:769). To make that
    // handoff work for every host, the validation runs under the EXACT
    // `ExtensionConfig` the effective host will request. Only hosts that
    // actually load the extension runtime validate eagerly: the
    // print/interactive/rpc mode hosts. Everything else (subcommands like
    // `list`/`install`, and the Server/Client hosts which load no extensions)
    // skips the load entirely — upstream subcommands never touch extensions.
    // Default-disabled packages (`@malinamnam/pi-phone`): one `[ext]`
    // diagnostic naming the exact opt-in, emitted only by hosts that load
    // extensions (subcommands stay quiet). The paths were already removed
    // from `extension_paths` before any runtime could start.
    if host_loads_extensions {
        pir::plugins::emit_default_disabled_package_diagnostics(&default_disabled_skips);
        // `--only-builtin` ignores on-disk `-e` paths (and every settings/
        // package extension). One diagnostic naming them, so a flag that
        // silently changes a user's plan is never silent. Builtin `-e`
        // entries are on the plan and are not reported.
        if !only_builtin_ignored.is_empty() {
            let ignored = only_builtin_ignored
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            pir::ext_diag!(
                "[ext] --only-builtin ignores {} on-disk extension path(s): {}",
                only_builtin_ignored.len(),
                ignored
            );
        } else if cli.only_builtin && !cli.no_extensions {
            // No on-disk `-e` path to name, but settings/package/discovered
            // extensions were still skipped by the flag: say so once.
            pir::ext_diag!(
                "[ext] --only-builtin: loading only the Rust builtin extensions; settings, \
                 package and discovered extensions are skipped"
            );
        }
    }
    // The interactive tool-selection resolver runs after this validation load
    // but before the TUI worker builds its session, so it must see the on-disk
    // extension tools the runtime just registered: resolving against a
    // core+builtin-only set dropped every name an on-disk extension declared
    // (`--tools my_js_tool` matched nothing and left an empty active override,
    // D10). Keep a clone of the parked runtime for that resolution; the TUI
    // host still consumes the parked handle, so the extension set is
    // evaluated once.
    #[cfg(feature = "tui")]
    let mut interactive_extension_runtime: Option<pir::extensions::ExtensionHandle> = None;
    // `--help-extensions`: captured from the validated runtime BEFORE it is
    // parked (the parked handle is consumed by the mode host, which this
    // metadata command never starts).
    let mut help_extension_flags: Vec<(pir::extensions::FlagDef, Option<String>)> = Vec::new();
    // Upstream `applyExtensionFlagValues` diagnostics (`runtime.diagnostics`):
    // an unregistered `--flag` or a string flag without a value is an ERROR
    // that aborts startup. Collected here so `--help-extensions` /
    // `--list-models` can print them and still exit 0 (upstream main.ts:881-895).
    let mut cli_flag_diagnostics: Option<Vec<pir::agent_session::CliExtensionFlagDiagnostic>> =
        None;
    if host_loads_extensions && !extension_paths.is_empty() {
        let validation_config =
            super::cli_preprocess::extension_runtime_config(rpc_command, interactive, &project_dir);
        // Package-contributed paths are warn-only: a package extension pir's
        // runtime cannot load (Node APIs, deeper bundler layouts) must not
        // brick startup. The strict gate still hard-fails the user's own
        // paths (`-e`, settings) with the upstream messaging.
        //
        // No model catalog here by design: this validation runs before any
        // provider environment is resolved (a broken models.json must not be
        // able to block extension validation), so the strict load passes
        // `None`. The host that consumes the parked runtime applies its
        // catalog via `load_extension_runtime_with_catalog`; only a factory
        // that resolves models during THIS first validation load can observe
        // the empty catalog, and every later load (child runtimes included)
        // pushes it pre-load.
        let package_paths = &package_resources.extensions;
        let validation = pir::agent_session::load_extension_runtime_strict(
            &extension_paths,
            validation_config.clone(),
        )
        .await;
        let validated = match validation {
            Ok((runtime, failures)) => {
                let mut fatal = false;
                for failure in &failures {
                    let is_package = failure
                        .path
                        .as_ref()
                        .is_some_and(|path| package_paths.contains(path));
                    fatal |= !is_package;
                    eprintln!("{}", failure.message);
                }
                if fatal {
                    eprintln!("Hint: Start without extensions using \"pir -ne\"");
                    std::process::exit(1);
                }
                runtime
            }
            Err(error) => {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
        };
        if let Some(runtime) = validated {
            // Validate the collected `--flag[=value]` arguments against the
            // loaded registrations. The values were already applied by the
            // strict load; re-applying is idempotent and lets this entry point
            // own the diagnostics.
            cli_flag_diagnostics = Some(pir::agent_session::apply_cli_extension_flags(Some(
                &runtime,
            )));
            if cli.help_extensions {
                help_extension_flags = runtime
                    .flags()
                    .into_iter()
                    .map(|flag| {
                        let owner = runtime.extension_identity(flag.ext_id);
                        (flag, owner)
                    })
                    .collect();
            }
            // The interactive resolver must see these tools' names/facts
            // (`runtime_available_tools`) before the TUI worker builds its
            // session; the parked clone below is the same runtime the host
            // consumes, so this adds no second load.
            #[cfg(feature = "tui")]
            {
                interactive_extension_runtime = Some(runtime.clone());
            }
            // Park the validated runtime: the host's own load call consumes
            // it (matching paths + config), so the extension set executes
            // once instead of twice.
            pir::agent_session::stash_validated_extension_runtime(
                &extension_paths,
                validation_config,
                runtime,
            );
        }
    }

    // Upstream `applyExtensionFlagValues` runs even when no extension plan was
    // loaded (no `-e`, `--no-extensions`, only builtins filtered away): every
    // collected `--flag` is then unregistered. The empty-plan case never
    // reaches `apply_cli_extension_flags(Some(..))` above.
    if cli_flag_diagnostics.is_none() && host_loads_extensions {
        cli_flag_diagnostics = Some(pir::agent_session::apply_cli_extension_flags(None));
    }
    let cli_flag_diagnostics = cli_flag_diagnostics.unwrap_or_default();
    let cli_flag_error = cli_flag_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.error);
    // `--help-extensions` / `--list-models` report the diagnostics and still
    // exit 0 (upstream `reportDiagnostics` before `printHelp` / `listModels`);
    // every mode host treats an error diagnostic as fatal (main.ts:918-926).
    if cli_flag_error || cli.help_extensions || cli.list_models.is_some() {
        for diagnostic in &cli_flag_diagnostics {
            eprintln!("Error: {}", diagnostic.message);
        }
    }

    // `--help-extensions`: the dynamic half of upstream's help (the flags the
    // loaded extensions registered). clap's `--help` exits before this point
    // and therefore never executes extension code; this opt-in command does,
    // by design (upstream's `--help` loads them too).
    if cli.help_extensions {
        return super::cli_commands::run_help_extensions(&help_extension_flags);
    }

    // `--list-models [search]`: print available models and exit. Reached only
    // when another metadata flag forced the regular path; the common case
    // returned from the fast path above.
    if let Some(search) = cli.list_models.as_deref() {
        return run_list_models(
            if search.is_empty() {
                None
            } else {
                Some(search)
            },
            &settings_manager,
        );
    }

    if cli_flag_error {
        std::process::exit(1);
    }

    // Startup phase report (PI_STARTUP_BENCHMARK=1 / PI_TIMING=1, upstream
    // main.ts:932 + core/timings.ts) — printed to stderr before the mode
    // host starts. Upstream's subcommands exit before `printTimings`
    // (main.ts:596-624), so the report (and the benchmark gate below) only
    // covers the mode-host commands.
    benchmark.time("startupSetup");
    let mode_host_command = matches!(
        cli.command,
        None | Some(Command::Interactive)
            | Some(Command::Print { .. })
            | Some(Command::Rpc)
            | Some(Command::Server { .. })
            | Some(Command::Client)
    );
    if mode_host_command {
        benchmark.print_to_stderr();
        // Upstream main.ts:932-934: PI_STARTUP_BENCHMARK only drives the
        // interactive init-then-stop cycle; any other mode is a hard error.
        let benchmark_interactive = match cli.command {
            None => entry.app_mode == AppMode::Interactive,
            Some(Command::Interactive) => mode != Some(Mode::Rpc),
            _ => false,
        };
        if pir::config::timings::startup_benchmark_enabled() && !benchmark_interactive {
            eprintln!("Error: PI_STARTUP_BENCHMARK only supports interactive mode");
            std::process::exit(1);
        }
    }

    match cli.command.clone() {
        Some(Command::Version) => {
            println!("pir {}", env!("CARGO_PKG_VERSION"));
            println!("Rust edition: 2021");
            println!("Protocol version: {}", pir::protocol::PROTOCOL_VERSION);
            println!(
                "Tool execution mode: {}",
                match config.tool_execution {
                    pir::config::ToolExecutionMode::Parallel => "parallel",
                    pir::config::ToolExecutionMode::Sequential => "sequential",
                }
            );
            println!("Max parallel tools: {}", config.max_parallel_tools);
            Ok(())
        }
        Some(Command::Config {
            local,
            json,
            enable,
            disable,
            source,
        }) => pir::coding_agent::package_manager::run_package_command_with_trust(
            &pir::coding_agent::package_manager::PackageCommand::Config {
                local,
                json,
                enable,
                disable,
                source,
            },
            cli.trust_override(),
        ),
        Some(Command::Install { source, local }) => {
            pir::coding_agent::package_manager::run_package_command_with_trust(
                &pir::coding_agent::package_manager::PackageCommand::Install { source, local },
                cli.trust_override(),
            )
        }
        Some(Command::Remove { source, local }) | Some(Command::Uninstall { source, local }) => {
            pir::coding_agent::package_manager::run_package_command_with_trust(
                &pir::coding_agent::package_manager::PackageCommand::Remove { source, local },
                cli.trust_override(),
            )
        }
        Some(Command::Update { .. }) => {
            // Upstream `pi update` resolves an UpdateTarget from the
            // `--models`/`--extensions`/`--all`/`--self`/`--extension` flags
            // and the positional source (package-manager-cli.ts:442-584);
            // the conditional conflicts (e.g. `self --extensions` is allowed
            // but `npm:foo --extensions` is not) are validated here and exit
            // 1 exactly like upstream's `conflictingOptions`.
            match cli.resolve_update_plan() {
                Ok(Some(plan)) => {
                    let target =
                        pir::coding_agent::package_manager::UpdateTarget::from(plan.request);
                    pir::coding_agent::package_manager::run_package_command_with_trust(
                        &pir::coding_agent::package_manager::PackageCommand::Update {
                            target,
                            force: plan.force,
                            show_extensions_skipped_note: plan.show_extensions_skipped_note,
                        },
                        cli.trust_override(),
                    )
                }
                Ok(None) => Ok(()),
                Err(message) => {
                    eprintln!("{message}");
                    eprintln!(
                        "Usage: pir update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
                    );
                    std::process::exit(1);
                }
            }
        }
        Some(Command::List) => pir::coding_agent::package_manager::run_package_command_with_trust(
            &pir::coding_agent::package_manager::PackageCommand::List,
            cli.trust_override(),
        ),
        Some(Command::Models { command }) => match command.unwrap_or(ModelsCommand::List) {
            ModelsCommand::List => {
                match pir::ai::providers::load_pi_config()? {
                    Some(configured) => {
                        println!(
                            "{}:{} ({})",
                            configured.model.provider, configured.model.id, configured.model.name
                        );
                    }
                    None => {
                        println!("No models configured. Add ~/.pi/agent/models.json.");
                    }
                }
                Ok(())
            }
        },
        Some(Command::Interactive) => {
            // Upstream `--mode rpc` switches the default command to RPC; RPC
            // is core, so it stays available in a build without the TUI.
            if mode == Some(Mode::Rpc) {
                return pir::coding_agent::rpc::run_rpc_with_options(
                    &extension_paths,
                    tool_selection_request.clone(),
                    rpc_overrides(&cli, &config)?,
                )
                .await;
            }
            // The interactive host is the terminal TUI; without that feature
            // the command parses but fails with the missing-feature error.
            #[cfg(not(feature = "tui"))]
            {
                Err(missing_feature("interactive mode", "tui"))
            }
            #[cfg(feature = "tui")]
            {
                // Upstream shows the collected deprecation warnings only in
                // interactive mode, before the TUI takes over the terminal.
                pir::config::migrations::show_deprecation_warnings(
                    &migrations.deprecation_warnings,
                );
                let plan = resolve_run_session_plan(&cli, &config, true)?;
                if plan == SessionPlan::Pick {
                    // The user cancelled the resume picker; nothing to attach.
                    return Ok(());
                }
                // `startup_settings_diagnostics` moves into the TUI as chat
                // warning notices; interactive mode intentionally does not
                // print them to stderr (see the collection site above).
                // The CLI model/credential/prompt/theme flags are resolved by
                // the TUI host itself (it owns the full model environment).
                pir::tui::run(
                    extension_paths,
                    cli.use_theme.clone(),
                    plan,
                    config.session_dir.clone(),
                    pir::coding_agent::resources::ResourcePathOptions {
                        skill_paths: cli.skill.clone(),
                        prompt_paths: cli.prompt_template.clone(),
                        no_skills: cli.no_skills,
                        no_prompt_templates: cli.no_prompt_templates,
                    },
                    Vec::new(),
                    resolve_interactive_tool_selection(
                        &cli,
                        &settings_manager,
                        interactive_extension_runtime.as_ref(),
                    )?,
                    cli.tui_mode.clone(),
                    &settings_manager,
                    &package_resources.themes,
                    startup_settings_diagnostics,
                    interactive_overrides(&cli)?,
                )
            }
        }
        Some(Command::Print { prompt }) => {
            // Upstream `--mode rpc` starts the RPC loop instead of printing.
            if mode == Some(Mode::Rpc) {
                return pir::coding_agent::rpc::run_rpc_with_options(
                    &extension_paths,
                    tool_selection_request.clone(),
                    rpc_overrides(&cli, &config)?,
                )
                .await;
            }
            // Headless resolution never yields the picker plan: `--resume`
            // selects this project's most recent session instead.
            let plan = resolve_run_session_plan(&cli, &config, false)?;
            run_print_mode(
                &cli,
                &config,
                &settings_manager,
                &extension_paths,
                pir::coding_agent::cli::PrintPrompts {
                    initial_message: Some(prompt),
                    additional_messages: Vec::new(),
                },
                Vec::new(),
                plan,
                mode == Some(Mode::Json),
            )
            .await
        }
        Some(Command::Rpc) => {
            pir::coding_agent::rpc::run_rpc_with_options(
                &extension_paths,
                tool_selection_request.clone(),
                rpc_overrides(&cli, &config)?,
            )
            .await
        }
        #[cfg(not(feature = "server"))]
        Some(Command::Server { .. }) => Err(missing_feature("pir server", "server")),
        #[cfg(feature = "server")]
        Some(Command::Server { unix }) => {
            let security = pir::server::ServerSecurityConfig {
                auth_token: config.server_auth_token.clone(),
                max_connections: config.server_max_connections,
                max_request_body_bytes: config.server_max_request_body_bytes,
                max_connection_body_bytes: config.server_max_connection_body_bytes,
                max_sessions: pir::server::ServerSecurityConfig::default().max_sessions,
                max_subscriptions: pir::server::ServerSecurityConfig::default().max_subscriptions,
                idle_timeout: pir::server::ServerSecurityConfig::default().idle_timeout,
                allow_remote_bash: config.allow_remote_bash,
                allow_remote_agent: config.allow_remote_agent,
            };
            // The global selection is resolved against the server's OWN
            // policy-filtered core registry (shell tools removed when
            // `allow_remote_bash` is off). Upstream has no server host, so
            // this is pir's consistency rule, not upstream parity.
            let tool_selection = pir::server::resolve_server_tool_selection(
                &tool_selection_request,
                config.allow_remote_bash,
            )
            .map_err(anyhow::Error::msg)?;
            if let Some(socket_path) = unix {
                #[cfg(unix)]
                {
                    // Persist sessions under the CLI-resolved session directory
                    // so a Unix-host restart reopens them instead of starting
                    // empty (upstream SessionRouter over a persisted ServerHost).
                    let state = pir::server::ServerState::default()
                        .with_session_dir(config.session_dir.clone())
                        .with_tool_selection(tool_selection.clone());
                    let server = pir::server::UnixServer::bind_with_state_and_security(
                        std::path::PathBuf::from(&socket_path),
                        state,
                        security,
                    )
                    .await?;
                    eprintln!(
                        "pir server listening on unix:{}",
                        server.socket_path().display()
                    );
                    return server.run().await.map_err(Into::into);
                }
                #[cfg(not(unix))]
                {
                    let _ = socket_path;
                    anyhow::bail!("--unix is only available on Unix platforms");
                }
            }
            // Session persistence is a host concern: bind the durable session
            // directory the CLI resolved (settings/env/flag) so `list` merges
            // on-disk sessions and attach reopens them across restarts.
            let state = pir::server::ServerState::default()
                .with_session_dir(config.session_dir.clone())
                .with_tool_selection(tool_selection);
            let server = pir::server::Server::bind_with_state_and_security(
                &config.server_address,
                state,
                security,
            )
            .await?;
            eprintln!("pir server listening on {}", server.local_addr()?);
            server.run().await.map_err(Into::into)
        }
        #[cfg(not(feature = "client"))]
        Some(Command::Client) => Err(missing_feature("pir client", "client")),
        #[cfg(feature = "client")]
        Some(Command::Client) => {
            let mut client = pir::client::Client::connect_with_token(
                &config.server_address,
                config.server_auth_token.as_deref(),
            )
            .await?;
            let sessions = client.list().await?;
            println!("{} sessions", sessions.len());
            Ok(())
        }
        Some(Command::Auth { command }) => {
            let _: () = match command.unwrap_or(AuthCommand::Status) {
                AuthCommand::Status => pir::ai::auth::cli::status()?,
                AuthCommand::Login {
                    provider,
                    agent_name,
                } => pir::ai::auth::cli::login(&provider, agent_name).await?,
                AuthCommand::Logout { provider } => pir::ai::auth::cli::logout(&provider)?,
                AuthCommand::Check {
                    provider,
                    model,
                    json,
                    credentials,
                    no_refresh,
                } => {
                    // Upstream `auth check` exit codes: 0 ready, 1 not_ready,
                    // 2 invalid (and a parse/validation error is 2 as well).
                    match pir::ai::auth::cli::check(
                        provider.as_deref(),
                        model.as_deref(),
                        json,
                        credentials,
                        no_refresh,
                    )
                    .await
                    {
                        Ok(code) if code != 0 => std::process::exit(code),
                        Ok(_) => {}
                        Err(error) => {
                            eprintln!("Error: {error}");
                            std::process::exit(2);
                        }
                    }
                }
                AuthCommand::PrintApiKey { provider, model } => {
                    pir::ai::auth::cli::print_api_key(provider.as_deref(), model.as_deref())
                        .await?;
                }
                AuthCommand::PrintBearerToken {
                    provider,
                    model,
                    min_expiry,
                } => {
                    pir::ai::auth::cli::print_bearer_token(
                        provider.as_deref(),
                        model.as_deref(),
                        min_expiry,
                    )
                    .await?;
                }
            };
            Ok(())
        }
        Some(Command::Mcp { command }) => {
            // No subcommand keeps pir's historical default: check every
            // configured server (`pir mcp`). `list` is its own upstream-shaped
            // report and returns 1 when a server or config file fails.
            let code = pir::coding_agent::cli::run_mcp_command_code_with_trust(
                command.unwrap_or(McpCommand::Check {
                    server: None,
                    json: false,
                }),
                cli.trust_override(),
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Some(Command::Grep { args }) => pir::native_cli::run_grep(&args),
        Some(Command::Sed { args }) => pir::native_cli::run_sed(&args),
        Some(Command::Python { args }) => pir::native_cli::run_python(&args),
        Some(Command::Session) => {
            let path = std::path::Path::new(&config.session_dir);
            let exists = path.is_dir();
            let count = if exists {
                std::fs::read_dir(path)
                    .map(|entries| entries.filter_map(std::result::Result::ok).count())
                    .unwrap_or(0)
            } else {
                0
            };
            println!("Session directory: {}", config.session_dir);
            println!("Exists: {exists}");
            println!("Entries: {count}");
            Ok(())
        }
        None => {
            // Upstream entry shapes (no subcommand given): print / json / rpc
            // per `resolveAppMode`, with the piped-stdin and positional
            // message prompt contract.
            let mut app_mode = entry.app_mode;
            // Piped stdin content (upstream `readPipedStdin`; RPC uses stdin
            // for the JSON-RPC protocol, so it is skipped there). A prompt on
            // stdin downgrades interactive to print (upstream main.ts:897).
            let stdin_content = if app_mode == AppMode::Rpc {
                None
            } else {
                pir::coding_agent::cli::read_piped_input(stdin_tty, std::io::stdin())
            };
            if stdin_content.is_some() && app_mode == AppMode::Interactive {
                app_mode = AppMode::Print;
            }
            match app_mode {
                AppMode::Rpc => {
                    pir::coding_agent::rpc::run_rpc_with_options(
                        &extension_paths,
                        tool_selection_request.clone(),
                        rpc_overrides(&cli, &config)?,
                    )
                    .await
                }
                AppMode::Print | AppMode::Json => {
                    // Headless resolution never yields the picker plan:
                    // `--resume` selects this project's most recent session.
                    let plan = resolve_run_session_plan(&cli, &config, false)?;
                    // `@file` image positionals become `initialImages`
                    // (upstream prepareInitialMessage) while still
                    // contributing their `<file name="…"></file>` marker to
                    // the prompt; text positionals render into the same
                    // in-order file text (upstream processFileArguments).
                    let (file_text, images) = pir::coding_agent::print::partition_file_args(
                        &entry.file_args,
                        &project_dir,
                    )?;
                    let prompts = pir::coding_agent::cli::build_print_prompts(
                        stdin_content.as_deref(),
                        &file_text,
                        &entry.messages,
                    );
                    if prompts.initial_message.is_none() {
                        if app_mode == AppMode::Json {
                            // Upstream print-mode.ts:122-127 still creates the
                            // session and writes its header when no prompt is
                            // given; the pass then exits without a run.
                            pir::coding_agent::print::run_empty_json_print(
                                &plan,
                                std::path::Path::new(&config.session_dir),
                                &project_dir.to_string_lossy(),
                            )?;
                            return Ok(());
                        }
                        // Upstream runs an empty print pass and exits 0.
                        eprintln!(
                            "pir: no prompt given (use -p <message>, positional messages, or piped stdin)"
                        );
                        return Ok(());
                    }
                    run_print_mode(
                        &cli,
                        &config,
                        &settings_manager,
                        &extension_paths,
                        prompts,
                        images,
                        plan,
                        app_mode == AppMode::Json,
                    )
                    .await
                }
                AppMode::Interactive => {
                    // Piped stdin downgrades to print above; a bare
                    // interactive invocation needs the TUI.
                    #[cfg(not(feature = "tui"))]
                    {
                        Err(missing_feature("interactive mode", "tui"))
                    }
                    #[cfg(feature = "tui")]
                    {
                        // Upstream shows the collected deprecation warnings
                        // only in interactive mode, before the TUI takes over.
                        pir::config::migrations::show_deprecation_warnings(
                            &migrations.deprecation_warnings,
                        );
                        let plan = resolve_run_session_plan(&cli, &config, true)?;
                        if plan == SessionPlan::Pick {
                            // The user cancelled the resume picker; nothing
                            // to attach.
                            return Ok(());
                        }
                        // `startup_settings_diagnostics` moves into the TUI as
                        // chat warning notices; interactive mode intentionally
                        // does not print them to stderr.
                        // Upstream initial message: `@file` text + the first
                        // message join with no separator (buildInitialMessage);
                        // remaining messages enqueue individually.
                        let file_text = pir::coding_agent::cli::process_file_arguments(
                            &entry.file_args,
                            &project_dir,
                        )?;
                        let prompts = pir::coding_agent::cli::build_print_prompts(
                            None,
                            &file_text,
                            &entry.messages,
                        );
                        let mut initial_messages: Vec<String> = Vec::new();
                        if let Some(combined) = prompts.initial_message {
                            initial_messages.push(combined);
                        }
                        initial_messages.extend(prompts.additional_messages);
                        pir::tui::run(
                            extension_paths,
                            cli.use_theme.clone(),
                            plan,
                            config.session_dir.clone(),
                            pir::coding_agent::resources::ResourcePathOptions {
                                skill_paths: cli.skill.clone(),
                                prompt_paths: cli.prompt_template.clone(),
                                no_skills: cli.no_skills,
                                no_prompt_templates: cli.no_prompt_templates,
                            },
                            initial_messages,
                            resolve_interactive_tool_selection(
                                &cli,
                                &settings_manager,
                                interactive_extension_runtime.as_ref(),
                            )?,
                            cli.tui_mode.clone(),
                            &settings_manager,
                            &package_resources.themes,
                            startup_settings_diagnostics,
                            interactive_overrides(&cli)?,
                        )
                    }
                }
            }
        }
    }
}

/// Whether this invocation is a normal startup whose upstream pi path creates
/// the agent-dir files (`auth.json` / `models-store.json`).
///
/// `--export` exits before the upstream runtime (`main.ts:640-652`), and the
/// native helper subcommands (`version`, `grep`, `sed`, `python`) are pir-only
/// surfaces that never read the pi config. `--version`/`-v` exit in clap
/// before this module runs at all.
fn bootstraps_agent_dir(cli: &Cli) -> bool {
    cli.export.is_none()
        && !matches!(
            cli.command,
            Some(
                Command::Version
                    | Command::Grep { .. }
                    | Command::Sed { .. }
                    | Command::Python { .. }
            )
        )
}

/// First-run setup (upstream `showFirstTimeSetup`, startup-ui.ts:182-219):
/// when the experimental gate holds (see `config::first_run`), ask for a theme
/// and the analytics opt-in on the terminal and persist both to the global
/// `settings.json`. The startup settings manager here is loaded with the
/// project scope untrusted — the setup writes only the global scope, and the
/// real manager is created afterwards. A cancel (EOF/out-of-range) leaves
/// settings untouched, exactly like upstream's cancel path.
fn run_first_time_setup_if_needed(agent_dir: &std::path::Path, project_dir: &std::path::Path) {
    use pir::config::first_run;
    let settings_path = agent_dir.join("settings.json");
    if !first_run::should_run_first_time_setup(
        &settings_path,
        first_run::experimental_enabled(),
        first_run::custom_agent_dir(),
    ) {
        return;
    }
    let mut startup_settings =
        SettingsManager::load(agent_dir, &project_dir.join(".pi"), project_dir, false);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    match first_run::prompt_first_time_setup(&mut reader, &mut writer) {
        Ok(Some(result)) => {
            for error in first_run::apply_first_time_setup(&mut startup_settings, &result) {
                eprintln!("Warning: {error}");
            }
        }
        // Cancelled: the next interactive launch asks again (settings.json
        // still absent), like upstream's `onCancel` → `finish(undefined)`.
        Ok(None) => {}
        Err(error) => eprintln!("Warning: first-run setup was skipped: {error}"),
    }
}

/// The CLI flags the interactive host must apply to the session it builds:
/// model/credential/thinking selection, the session name, the prompt
/// overrides and the theme paths.
///
/// Only the interactive TUI host calls this (`pir::tui::TuiOverrides` lives
/// behind the `tui` feature), so the function carries the same gate: a
/// `cli`-only binary compiles without the TUI module.
#[cfg(feature = "tui")]
fn interactive_overrides(cli: &Cli) -> Result<pir::tui::TuiOverrides> {
    Ok(pir::tui::TuiOverrides {
        model: pir::coding_agent::overrides::CliModelOverrides {
            provider: cli.provider.clone(),
            model: cli.model.clone(),
            thinking: cli.thinking_level(),
            api_key: cli.api_key.clone(),
            model_patterns: cli.models_patterns(),
            is_continuing: cli.continue_session || cli.resume,
        },
        session_name: cli.name.clone(),
        theme_paths: cli.theme.clone(),
        no_themes: cli.no_themes,
        system_prompt: cli
            .system_prompt
            .as_deref()
            .map(pir::coding_agent::overrides::resolve_prompt_input),
        append_system_prompt: cli
            .append_system_prompt
            .iter()
            .map(|entry| pir::coding_agent::overrides::resolve_text_or_file(entry))
            .collect::<Result<Vec<_>>>()?,
    })
}

/// The CLI flags the RPC host applies to every session its factory builds,
/// plus the CLI session plan the host attaches at startup (upstream
/// `main.ts:950-952` hands the session selected/created by the CLI to
/// `runRpcMode`).
fn rpc_overrides(
    cli: &Cli,
    config: &pir::config::Config,
) -> Result<pir::coding_agent::rpc::RpcOverrides> {
    let plan = resolve_run_session_plan(cli, config, false)?;
    Ok(pir::coding_agent::rpc::RpcOverrides {
        model: pir::coding_agent::overrides::CliModelOverrides {
            provider: cli.provider.clone(),
            model: cli.model.clone(),
            thinking: cli.thinking_level(),
            api_key: cli.api_key.clone(),
            model_patterns: cli.models_patterns(),
            is_continuing: cli.continue_session || cli.resume,
        },
        system_prompt: cli
            .system_prompt
            .as_deref()
            .map(pir::coding_agent::overrides::resolve_prompt_input),
        append_system_prompt: cli
            .append_system_prompt
            .iter()
            .map(|entry| pir::coding_agent::overrides::resolve_text_or_file(entry))
            .collect::<Result<Vec<_>>>()?,
        session_name: cli.name.clone(),
        // A headless host never sees the picker plan; guard anyway so a
        // future caller cannot park an unresolved plan on the RPC host.
        startup_session: match plan {
            SessionPlan::Pick => None,
            plan => Some(plan),
        },
    })
}
