use super::*;

use pir::coding_agent::package_manager::TopLevelResourceFilter;

/// Resolve extension paths for print/RPC/interactive hosts.
///
/// The unified loader ordering (`pir::plugins::loader::order_extension_paths`):
/// explicit `-e` CLI paths first (including `builtin:<name>` entries), then
/// the settings `extensions` path lists plus the default discovery paths with
/// project-local BEFORE user-global (the upstream discovery-order fix), deduped
/// by canonical path. `--no-extensions` keeps only the CLI paths (upstream
/// semantics: an explicit `-e` still loads). `--no-mcp` / the disabled-builtin
/// list filter `builtin:<name>` entries even when `-e` enables them (upstream
/// `disabledBuiltinExtensions`).
///
/// The top-level settings `extensions` arrays act as a source + filter layer,
/// one per scope (upstream `resolve()` -> `resolveLocalEntries` +
/// `addAutoDiscoveredResources`, package-manager.ts:946-976/2396-2479): plain
/// entries are sources resolved against that scope's base directory (upstream
/// `resolvePathFromBase`), while the same scope's `!`/`+`/`-` entries decide
/// which of them survive (upstream `splitPatterns` + `applyPatterns`, compiled
/// by [`TopLevelResourceFilter`]) — the same per-scope pipeline
/// `pir::coding_agent::resources` runs for `skills`/`prompts`. CLI paths and
/// installed packages are never filtered (upstream `resolveExtensionSources` /
/// package filters are separate layers).
///
/// The default-disabled package skips
/// (`pir::plugins::DEFAULT_DISABLED_PACKAGE_NAMES`): the paths were already
/// dropped from the plan before any runtime exists, and the mode host emits
/// the `[ext]` diagnostic only when it actually loads extensions.
///
/// `only_builtin_ignored` carries on-disk `-e` paths ignored by
/// `--only-builtin` (the host emits one diagnostic when it loads extensions).
/// `--no-extensions` wins when both flags are set
/// ([`pir::plugins::extension_plan_mode`]).
///
/// `package_resources` is the single package scan this resolution performed
/// (`PackageResources::default()` for the `--only-builtin` plan, which never
/// scans installed packages). Callers reuse it for the TUI theme list instead
/// of resolving every package a second time.
#[derive(Default)]
pub(super) struct ResolvedExtensionPaths {
    pub paths: Vec<std::path::PathBuf>,
    pub default_disabled_skips: Vec<pir::plugins::DefaultDisabledPackageSkip>,
    pub only_builtin_ignored: Vec<std::path::PathBuf>,
    pub package_resources: pir::coding_agent::package_manager::PackageResources,
}

pub(super) fn resolve_extension_paths(
    cli: &Cli,
    settings: &pir::config::settings::SettingsManager,
    project_trusted: bool,
) -> ResolvedExtensionPaths {
    resolve_extension_paths_in(
        cli,
        settings,
        project_trusted,
        pir::coding_agent::context::agent_dir(),
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    )
}

/// [`resolve_extension_paths`] with the ambient agent/project directories
/// passed in (keeps the resolution testable without touching the process
/// environment).
fn resolve_extension_paths_in(
    cli: &Cli,
    settings: &pir::config::settings::SettingsManager,
    project_trusted: bool,
    agent_dir: std::path::PathBuf,
    project_dir: std::path::PathBuf,
) -> ResolvedExtensionPaths {
    // `project_trusted` IS the run's resolved decision: the startup gate already
    // folded in `--approve`/`--no-approve`, `PIR_TRUST_PROJECT=1` and the
    // extension/store/policy order before pinning it
    // (`run_mode.rs` -> `pin_decision`). Re-reading `PIR_TRUST_PROJECT` here
    // would re-OR the operator override after a resolved `--no-approve` and
    // load the project scope the run explicitly denied.
    let trust_project = project_trusted;
    // Package-contributed resources are resolved ONCE here: the extension
    // ordering consumes `.extensions` and the caller reuses the same snapshot
    // for the TUI theme list, instead of walking every installed package a
    // second time on startup. The `--only-builtin` plan never looks at
    // installed packages, so it keeps the default (empty) snapshot.
    let mut package_resources = pir::coding_agent::package_manager::PackageResources::default();
    // Upstream `resolveCliPaths(cwd, parsed.extensions)` (main.ts:556-558,
    // 729): a local `-e` value is resolved to an absolute path (`~` and
    // `file://` expanded, relative entries against cwd) BEFORE the plan is
    // ordered, so a later cwd change cannot reinterpret it; package sources
    // and `builtin:<name>` pass through verbatim (`isLocalPath`).
    let cli_extension_paths: Vec<std::path::PathBuf> = cli
        .extension
        .iter()
        .map(|path| {
            let text = path.to_string_lossy();
            if pir::coding_agent::cli::is_local_cli_path(&text) {
                pir::coding_agent::cli::resolve_cli_path(&text, &project_dir)
            } else {
                path.clone()
            }
        })
        .collect();
    // `--only-builtin` short-circuits discovery entirely: no settings scope
    // expansion, no auto-directory probing, no installed-package scan. The
    // plan is CLI `builtin:<name>` entries plus the default builtins, and any
    // on-disk `-e` path is ignored (returned for the host's one-line
    // diagnostic). `--no-extensions` takes precedence when both are set.
    let plan_mode = pir::plugins::extension_plan_mode(cli.no_extensions, cli.only_builtin);
    let (ordered, only_builtin_ignored) = if plan_mode
        == pir::plugins::ExtensionPlanMode::OnlyBuiltin
    {
        pir::plugins::only_builtin_paths(&cli_extension_paths)
    } else {
        let project_base = project_dir.join(".pi");
        // Installed packages (settings.packages, upstream resource-loader.ts:529
        // via DefaultPackageManager.resolve()) are scanned ONCE: `.extensions`
        // feed the ordering below, and the caller reuses the full snapshot for
        // the TUI theme list. The CLI host strict-validates only the user's
        // own paths; package paths load warn-only (a Node-runtime package
        // pir's QuickJS can never load must not block startup — upstream
        // reports load errors and continues).
        package_resources = pir::coding_agent::package_manager::resolve_package_resources(
            settings,
            &agent_dir,
            &project_dir,
            trust_project,
        );

        let mut defaults: Vec<std::path::PathBuf> = Vec::new();
        // `--no-extensions` keeps only explicit `-e` paths: every default
        // source below is dropped (`order_extension_paths(..., true)`), so
        // skip the settings/auto-directory scan instead of gathering a list
        // that is discarded. Package resources were still collected above
        // because themes are not extensions.
        if !cli.no_extensions {
            // One entry list per scope, like the upstream `resolve()` pass (an
            // untrusted project scope is already empty in the manager).
            let project_entries = ExtensionScopeEntries::from_raw(&settings.project_raw);
            let user_entries = ExtensionScopeEntries::from_raw(&settings.global_raw);

            // Upstream `resourcePrecedenceRank` order: project settings paths
            // (trust gated), project auto dir, user settings paths, user auto
            // dir. Packages rank below all of them and append last.
            if trust_project {
                defaults.extend(project_entries.source_paths(&project_base));
            }
            #[cfg(feature = "extensions")]
            let auto_dirs = pir::extensions::loader::default_extension_paths(
                &agent_dir,
                &project_dir,
                trust_project,
            );
            // Auto-discovery directories are pure path computation; the
            // `extensions` module's loader is engine-gated, so a headless
            // build computes the same list here (project first, trusted only;
            // then user — parity with
            // `extensions::loader::default_extension_paths`).
            #[cfg(not(feature = "extensions"))]
            let auto_dirs = {
                let mut paths = Vec::new();
                if trust_project {
                    paths.push(project_dir.join(".pi").join("extensions"));
                }
                paths.push(agent_dir.join("extensions"));
                paths
            };
            // The helper lists the project auto dir first when the project is
            // trusted, the user-global one last.
            let project_auto_dir = auto_dirs.first().filter(|_| trust_project);
            let user_auto_dir = auto_dirs.last();
            if let Some(path) = project_auto_dir.filter(|path| path.exists()) {
                defaults.push(path.clone());
            }
            defaults.extend(user_entries.source_paths(&agent_dir));
            if let Some(path) = user_auto_dir.filter(|path| path.exists()) {
                defaults.push(path.clone());
            }
            defaults.extend(package_resources.extensions.iter().cloned());
        }

        // Upstream ordering: CLI paths first, then settings/default paths
        // (project before global); `--no-extensions` drops the defaults but an
        // explicit `-e` still loads.
        let ordered =
            pir::plugins::order_extension_paths(&cli_extension_paths, &defaults, cli.no_extensions);
        (ordered, Vec::new())
    };

    // Default-disabled packages (`@malinamnam/pi-phone`): dropped from the
    // plan BEFORE any runtime starts. An explicit `-e` path naming the
    // package wins; otherwise the global `defaultDisabledPackagesEnabled`
    // setting turns the set on. The skips are emitted by the mode host only
    // when it actually loads extensions (subcommands stay quiet). Builtin-only
    // plans contain no package path, so this is a cheap no-op there.
    let (ordered, default_disabled_skips) = pir::plugins::filter_default_disabled_packages(
        &ordered,
        &cli_extension_paths,
        settings.default_disabled_packages_enabled(),
    );

    // `disabledBuiltinExtensions` (the `--no-mcp` → ["mcp"] pattern): the
    // settings key (UNIONED across the global and trusted-project scopes, so
    // a project can add names but never re-enable a globally disabled
    // builtin) composed with the flag names, plus `pythonTool: false` folded
    // in as `python`. Settings and flags only ever ADD names here. The filter
    // runs after both plan branches, so an explicit `-e builtin:<name>` or
    // `--only-builtin` cannot resurrect a disabled builtin (upstream
    // `disabledBuiltinExtensions` semantics).
    let disabled: std::collections::BTreeSet<String> =
        settings.disabled_builtins_with_flags(&pir::plugins::disabled_from_flags(cli.no_mcp));
    ResolvedExtensionPaths {
        paths: pir::plugins::filter_disabled_builtins(&ordered, &disabled),
        default_disabled_skips,
        only_builtin_ignored,
        package_resources,
    }
}

/// One scope's top-level `extensions` settings array, split into the plain
/// source entries and the compiled `!`/`+`/`-` override layer (upstream
/// `splitPatterns` + `getOverridePatterns`, package-manager.ts:311-322,
/// 720-745; the same split `coding_agent::resources` applies to
/// `skills`/`prompts`).
struct ExtensionScopeEntries {
    plain: Vec<String>,
    filter: TopLevelResourceFilter,
}

impl ExtensionScopeEntries {
    /// Read one scope's raw settings document (`SettingsManager::project_raw`
    /// / `global_raw`; an untrusted project scope is already empty).
    fn from_raw(raw: &serde_json::Value) -> Self {
        let entries: Vec<String> = raw
            .get("extensions")
            .and_then(serde_json::Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // Plain entries are sources, never filters (a `!`/`+`/`-` entry
        // selects nothing on its own — upstream `splitPatterns`).
        let filter = TopLevelResourceFilter::from_entries(&entries);
        let plain = entries
            .into_iter()
            .filter(|entry| {
                !entry.starts_with('!') && !entry.starts_with('+') && !entry.starts_with('-')
            })
            .collect();
        Self { plain, filter }
    }

    /// The scope's source paths: every plain entry resolves against the
    /// scope's base directory (upstream `resolvePathFromBase`) and survives
    /// only when the scope's own override patterns keep it (upstream
    /// `resolveLocalEntries` -> `applyPatterns`). A plain DIRECTORY entry is
    /// filtered as the directory root; upstream filters the extension files
    /// the directory expands to, and that expansion happens downstream in
    /// `discover_extensions`, which consumes this list.
    fn source_paths(&self, base_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        self.plain
            .iter()
            .map(|entry| resolve_scope_entry(entry, base_dir))
            .filter(|path| self.filter.keeps(path, base_dir))
            .collect()
    }
}

/// Resolve one settings entry against its scope's base directory (upstream
/// `resolvePathFromBase`; the same rule as `resources::resolve_entry` for
/// `skills`/`prompts`): `~` expands against the home directory, an absolute
/// path stays put, and a relative path resolves against the scope base
/// (`<cwd>/.pi` for the project scope, the agent dir for the user one).
fn resolve_scope_entry(entry: &str, base_dir: &std::path::Path) -> std::path::PathBuf {
    let expanded = pir::coding_agent::resources::expand_tilde(entry);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// Resolve the session plan for a run (`-c/-r/--session/--session-id/
/// --fork/--no-session`, same rules for every host). In the interactive host
/// `--resume` runs the full-screen picker here, BEFORE the TUI starts, and
/// folds the picked file into an [`SessionPlan::Open`] attach (one flow, no
/// separate "not wired" notice path). Headless hosts never see
/// [`SessionPlan::Pick`]: `resolve_session_plan` resumes the project's most
/// recent session for `--resume` instead.
pub(super) fn resolve_run_session_plan(
    cli: &Cli,
    config: &pir::config::Config,
    interactive: bool,
) -> Result<SessionPlan> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let session_dir = std::path::Path::new(&config.session_dir);
    let plan = resolve_session_plan(&cli.session_flags(), &cwd, session_dir, interactive)?;
    if !(interactive && plan == SessionPlan::Pick) {
        return Ok(plan);
    }
    // `Pick` is only produced for an interactive host, whose TUI owns the
    // picker. A build without `tui` never reaches this branch (the callers
    // return the missing-feature error first), so the plan is passed through.
    #[cfg(not(feature = "tui"))]
    return Ok(plan);
    #[cfg(feature = "tui")]
    {
        let cwd_text = cwd.to_string_lossy().into_owned();
        let sessions =
            pir::session_manager::SessionManager::list_sessions(session_dir, Some(&cwd_text));
        if sessions.is_empty() {
            eprintln!("pir: no sessions found for this directory; starting a new session");
            return Ok(SessionPlan::New { id: None });
        }
        match pir::coding_agent::session_pick::pick_session_with_all_checked(&sessions, &sessions) {
            Ok(Some(path)) => Ok(SessionPlan::Open(path)),
            // The user cancelled the picker: leave without starting the TUI.
            Ok(None) => Ok(SessionPlan::Pick),
            // The picker could not run (no TTY, raw mode unavailable): this
            // must be a diagnostic, not a silent exit 0 that looks like the
            // user cancelled.
            Err(message) => Err(anyhow::anyhow!("{message}")),
        }
    }
}

/// Resolve the effective session directory: `--session-dir` flag first, then
/// `PI_CODING_AGENT_SESSION_DIR`, then the settings `sessionDir`, then the
/// default layout (upstream config.ts env resolution).
///
/// Every source goes through the upstream `normalizePath`/`resolvePath` rules
/// (main.ts:691-695, utils/paths.ts:76-107): `~`/`~/`/`~\` expands against the
/// home directory, `file://` URLs convert to a filesystem path, and a relative
/// value resolves against the process cwd. Without this a literal `~/x` became
/// a directory named `~` under the cwd.
pub(super) fn resolve_session_dir(
    cli: &Cli,
    settings: &SettingsManager,
    default_dir: &str,
) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let resolve = |dir: &str| {
        pir::coding_agent::cli::resolve_cli_path(dir, &cwd)
            .to_string_lossy()
            .into_owned()
    };
    if let Some(dir) = &cli.session_dir {
        return resolve(dir);
    }
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR") {
        if !dir.is_empty() {
            return resolve(&dir);
        }
    }
    if let Ok(dir) = std::env::var("PIR_SESSION_DIR") {
        if !dir.is_empty() {
            return resolve(&dir);
        }
    }
    if let Some(dir) = &settings.settings.session_dir {
        if !dir.is_empty() {
            return resolve(dir);
        }
    }
    resolve(default_dir)
}

/// Run the pre-trust extension pass (upstream
/// `ResourceLoader.loadProjectTrustExtensions`, resource-loader.ts:532-538):
/// build the extension plan with the project scope FORCED untrusted — CLI
/// `-e` paths, user/global settings extensions, user auto-discovery and
/// user-scope packages — and load it so the `project_trust` handlers can
/// decide the project BEFORE the trust store / `defaultProjectTrust` policy
/// is consulted. The runtime is deliberately disposable: only the recorded
/// decision (and a `remember: true` store write) outlives it, and the host
/// later loads the FINAL plan with the resolved trust. Upstream runs the same
/// two passes.
///
/// No-op when the untrusted plan is empty. Load errors stay non-fatal here:
/// the strict validation pass against the final plan reports them once with
/// the upstream `-ne` hint.
pub(super) async fn bootstrap_project_trust_extensions(
    cli: &Cli,
    settings: &SettingsManager,
    config: pir::extensions::ExtensionConfig,
) {
    let plan = resolve_extension_paths(cli, settings, false);
    if plan.paths.is_empty() {
        return;
    }
    // Drop the handle immediately; the recorder in
    // `extensions::project_trust` already holds the decision.
    let _ = pir::agent_session::load_extension_runtime(&plan.paths, config).await;
}

/// The `ExtensionConfig` every CLI host uses for the strict validation load
/// (and for the pre-trust bootstrap pass, which must match it): RPC and the
/// interactive TUI carry a UI, print/json do not; all three receive the
/// project cwd so `ctx.cwd` is right even before a session exists.
pub(super) fn extension_runtime_config(
    rpc_command: bool,
    interactive: bool,
    project_dir: &std::path::Path,
) -> pir::extensions::ExtensionConfig {
    if rpc_command {
        pir::extensions::ExtensionConfig {
            mode: "rpc".to_string(),
            has_ui: true,
            ..Default::default()
        }
    } else if interactive {
        pir::extensions::ExtensionConfig {
            mode: "interactive".to_string(),
            has_ui: true,
            ..Default::default()
        }
    } else {
        pir::extensions::ExtensionConfig {
            cwd: project_dir.to_path_buf(),
            mode: "print".to_string(),
            ..Default::default()
        }
    }
}

/// Resolve project trust for this run (upstream `resolveProjectTrusted`):
/// `--approve`/`--no-approve` override everything; the
/// `PIR_TRUST_PROJECT=1` operator override and a `project_trust` decision
/// recorded by the extension bootstrap pass come next (extensions are
/// consulted BEFORE the store, `core/project-trust.ts:55-70`); a project
/// without trust-requiring resources is implicitly trusted; the
/// `trust.json` nearest-ancestor decision comes after that; the
/// `defaultProjectTrust` setting decides the rest — `ask` shows the selector
/// when a UI is available and resolves to "not trusted" in headless mode.
/// Returns the trusted flag.
///
/// The decision (and its selector, when one runs) is resolved BEFORE the
/// caller's command executes; `has_ui` is the caller's invocation policy
/// (`Cli::trust_prompt_has_ui`: metadata commands pass `false` even on a TTY,
/// upstream `trustPromptMode`).
///
/// The extension decision exists only after the bootstrap runtime loaded the
/// user/CLI extensions ([`bootstrap_project_trust_extensions`]); a call made
/// without that pass (no user extensions, no trust-requiring project
/// resources, an explicit CLI override, or `PIR_TRUST_PROJECT=1`) falls
/// through to the store/policy exactly as before.
pub(super) fn resolve_project_trust_for_run(
    cli: &Cli,
    agent_dir: &std::path::Path,
    has_ui: bool,
) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Some(trusted) = cli.trust_override() {
        return trusted;
    }
    if let Some(trusted) = pir::extensions::project_trust::override_or_extension_decision(&cwd) {
        return trusted;
    }
    let store = ProjectTrustStore::new(agent_dir);
    let default_trust =
        SettingsManager::load(agent_dir, &cwd.join(".pi"), &cwd, false).default_project_trust();
    resolve_run_trust(&cwd, &store, None, default_trust, has_ui, |options| {
        // Interactive `ask`: present the trust selector on the terminal
        // (upstream trust-manager.ts:67-97); the chosen option is
        // persisted by `resolve_run_trust`.
        let prompt = format!(
            "Trust project folder?\n{}\n\nThis allows pir to load .pi settings and resources and execute project extensions.",
            cwd.display()
        );
        println!("{prompt}");
        for (index, option) in options.iter().enumerate() {
            println!("  {}. {}", index + 1, option.label);
        }
        let mut choice = String::new();
        if std::io::stdin().read_line(&mut choice).is_err() {
            return None;
        }
        match choice.trim().parse::<usize>() {
            Ok(picked) if picked >= 1 => Some(picked - 1),
            _ => None,
        }
    })
    .trusted
}
