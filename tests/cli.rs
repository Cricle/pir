//! CLI integration tests: subcommand surface and exit semantics.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pir"));
    // Hermetic agent dir: the real ~/.pi may carry packages/extensions that
    // change the `config` listing (and must never leak into the test).
    cmd.env("PIR_AGENT_DIR", temp_agent_dir());
    cmd.args(args);
    cmd.output().expect("pir binary should start")
}

fn temp_agent_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-cli-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[test]
fn version_outputs_core_fields() {
    let output = run(&["version"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("pir"));
    assert!(text.contains("Protocol version: 8"));
}

#[test]
fn config_echoes_defaults() {
    let output = run(&["config"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    // The resource listing contract (package-manager wave): hermetic agent
    // dir => no packages/extensions, and the read-only footer.
    assert!(text.contains("Resource configuration"));
    assert!(text.contains("extensions: (none)"));
}

#[test]
fn config_json_prints_the_machine_readable_surface() {
    let output = run(&["config", "--json"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("config --json must print JSON");
    assert_eq!(value["writeScope"], "global");
    assert!(value["packages"].is_array());
    assert!(value["resources"].is_array());
}

#[test]
fn config_enable_without_a_matching_resource_fails() {
    let output = run(&["config", "--enable", "extensions:extensions/missing.js"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("No extensions resource"),
        "unexpected: {text}"
    );
}

#[test]
fn help_lists_every_top_level_subcommand() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in [
        "interactive",
        "print",
        "rpc",
        "server",
        "client",
        "auth",
        "models",
        "session",
        "config",
        "version",
    ] {
        assert!(text.contains(command), "help omitted {command}: {text}");
    }
}

#[test]
fn help_lists_global_extension_flag() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("-e, --extension"),
        "help omitted the extension flag: {text}"
    );

    // The flag is global, so every subcommand accepts it too (upstream
    // `pi -e ./ext.ts` style).
    for subcommand in ["print", "rpc"] {
        let sub = run(&[subcommand, "--help"]);
        assert!(sub.status.success());
        let sub_text = String::from_utf8_lossy(&sub.stdout);
        assert!(
            sub_text.contains("--extension"),
            "{subcommand} --help omitted the extension flag: {sub_text}"
        );
    }
}

#[test]
fn help_lists_extension_package_theme_and_session_flags() {
    // Upstream args.ts:300-346 (the Options block). Every extension/package/
    // theme/session flag pir actually parses must be discoverable in BOTH the
    // long (`--help`) and short (`-h`) help; clap prints the same option list
    // for both, and the audit item (#16) is specifically about discoverability.
    let flags = [
        "-e, --extension",
        "--no-extensions",
        "--only-builtin",
        "--append-system-prompt",
        "--system-prompt",
        "--models",
        "--theme",
        "--use-theme",
        "--no-themes",
        "--session",
        "--session-id",
        "--session-dir",
        "--no-session",
        "--resume",
        "--fork",
        "--continue",
        "--api-key",
        "--name",
        "--thinking",
        "--skill",
        "--no-skills",
        "--prompt-template",
        "--no-prompt-templates",
        "--no-context-files",
        "--no-mcp",
    ];
    for help_flag in ["--help", "-h"] {
        let output = run(&[help_flag]);
        assert!(
            output.status.success(),
            "pir {help_flag} must exit 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        for flag in flags {
            assert!(
                text.contains(flag),
                "pir {help_flag} omitted {flag}:\n{text}"
            );
        }
    }
}

#[test]
fn help_lists_pi_environment_variables() {
    // Upstream args.ts:418-467. The section is emitted through clap's
    // `after_help`, so both help spellings carry it.
    let variables = [
        "Environment Variables:",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "GEMINI_API_KEY",
        "AZURE_OPENAI_BASE_URL",
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_GATEWAY_ID",
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_REGION",
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "PI_PACKAGE_DIR",
        "PI_OFFLINE",
        "PI_SKIP_VERSION_CHECK",
        "PI_TELEMETRY",
        "PI_SHARE_VIEWER_URL",
        "PIR_TOKIO_WORKERS",
        "PIR_TOKIO_MAX_BLOCKING",
        "PIR_ENV_DAEMON_WORKERS",
        "PIR_ENV_DAEMON_MAX_LONG_REQUESTS",
    ];
    for help_flag in ["--help", "-h"] {
        let output = run(&[help_flag]);
        assert!(
            output.status.success(),
            "pir {help_flag} must exit 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        for variable in variables {
            assert!(
                text.contains(variable),
                "pir {help_flag} omitted {variable}:\n{text}"
            );
        }
        // The blocking-pool default is clamped to a floor of 4 (two per core
        // on a 1-2 core device), not only capped at 32; the help must match
        // `tokio_max_blocking_threads_from`.
        assert!(
            text.contains("min 4, max 32"),
            "pir {help_flag} must state the blocking-pool floor and ceiling:\n{text}"
        );
    }
}

#[test]
fn config_help_lists_resource_management_flags() {
    // `pir config` grew the non-interactive read/write surface
    // (`--json/--enable/--disable/--source`); it must stay discoverable in
    // both help spellings alongside the upstream `-l/--local` scope flag.
    let flags = ["-l, --local", "--json", "--enable", "--disable", "--source"];
    for help_flag in ["--help", "-h"] {
        let output = run(&["config", help_flag]);
        assert!(
            output.status.success(),
            "pir config {help_flag} must exit 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        for flag in flags {
            assert!(
                text.contains(flag),
                "pir config {help_flag} omitted {flag}:\n{text}"
            );
        }
    }
}

#[test]
fn invalid_subcommand_has_clap_error_exit_code() {
    // Upstream CLI contract: an unrecognised word is a PROMPT (positional
    // message), not a clap error — `pi does-not-exist` prints with that
    // message. Only clap's own surface (--help variants, malformed flags)
    // exits with the clap error code. Assert the prompt path: with no
    // provider configured the run fails on model resolution, not on
    // argument parsing.
    let home = std::env::temp_dir().join(format!("pir-cli-bad-sub-{}", std::process::id()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args(["does-not-exist"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_AGENT_DIR", &home);
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let output = command.output().expect("pir binary should start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unrecognized subcommand"),
        "unknown words are prompts, not clap errors: {stderr}"
    );
    assert_ne!(
        output.status.code(),
        Some(2),
        "exit 2 is reserved for clap errors, got: {stderr}"
    );
}

#[test]
fn print_requires_a_configured_provider() {
    let home = std::env::temp_dir().join(format!("pir-cli-no-provider-{}", std::process::id()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args(["print", "hello world"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        // Point the agent dir at the empty sandbox: no auth.json, no
        // settings.json, no trust decisions.
        .env("PIR_AGENT_DIR", &home);
    // The resolution order falls back to "first model with auth", so every
    // provider environment credential must be cleared too.
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let output = command.output().expect("pir binary should start");
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("no model available"), "got: {text}");
}

#[test]
fn api_key_requires_an_explicit_model() {
    // Upstream main.ts:831-839: an inline key must be scoped by --model,
    // --provider/--model, or --models instead of silently targeting the
    // settings default provider.
    let output = run(&["print", "hello world", "--api-key", "sk-test"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("--api-key requires a model to be specified"),
        "got: {text}"
    );
}

#[test]
fn json_mode_without_a_prompt_still_emits_the_session_header() {
    // Upstream print-mode.ts:122-127: an empty JSON pass creates the session
    // and writes its header before exiting.
    let home = std::env::temp_dir().join(format!("pir-cli-empty-json-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .args(["--mode", "json"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_AGENT_DIR", &home)
        .output()
        .expect("pir binary should start");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .unwrap_or_else(|| panic!("no session header line: {stdout:?}"));
    let header: serde_json::Value = serde_json::from_str(line).expect("header must be a JSON line");
    assert_eq!(header["type"], "session", "got: {header}");
    assert!(header["id"].is_string(), "got: {header}");
    assert!(header["cwd"].is_string(), "got: {header}");
}

#[test]
fn models_list_reports_configured_state() {
    let output = run(&["models", "list"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    // Built-in catalogs were removed by scope decision: the command reports
    // the configured models.json provider, or how to configure one.
    assert!(
        text.contains("No models configured") || text.contains(':'),
        "unexpected models output: {text}"
    );
}

#[test]
fn auth_reports_env_status() {
    let output = run(&["auth"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for variable in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "XAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "GROQ_API_KEY",
        "MISTRAL_API_KEY",
        "FIREWORKS_API_KEY",
        "TOGETHER_API_KEY",
        "NVIDIA_API_KEY",
        "HF_TOKEN",
        "MOONSHOT_API_KEY",
        "ZAI_API_KEY",
        "KIMI_API_KEY",
        "MINIMAX_API_KEY",
        "META_API_KEY",
        "CLOUDFLARE_API_KEY",
        "COPILOT_GITHUB_TOKEN",
        "OPENROUTER_API_KEY",
        "AZURE_OPENAI_API_KEY",
    ] {
        assert!(text.contains(variable), "auth omitted {variable}: {text}");
    }
}

#[test]
fn session_reports_directory_state() {
    let output = run(&["session"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Session directory:"));
}

#[test]
fn interactive_is_default_when_no_args_are_given() {
    // No-arg run must not panic at parse time. On non-interactive CI the
    // TUI will error quickly because stdin is not a terminal; capture that.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("pir binary should start");
    // Poll briefly for an early exit instead of a fixed multi-second sleep:
    // an exit must not be a parse failure (exit code 2); a process still
    // running after the window counts as a started TUI.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let status = loop {
        let status = child.try_wait().expect("check process status");
        if status.is_some() || std::time::Instant::now() >= deadline {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    if let Some(code) = status.map(|s| s.code()) {
        // If it exited, it must not be a parse failure (exit code 2).
        assert!(code != Some(2), "no-args run failed at parse stage");
        // A TUI error (exit 1) on non-tty is acceptable.
    }
    // If still running, the TUI started successfully; kill it.
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Argument preprocessing (migrated from src/coding_agent/cli.rs inline tests)
// ---------------------------------------------------------------------------

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

#[test]
fn preprocess_normalizes_bundled_shorts() {
    let (cleaned, unknown) = pir::coding_agent::cli::preprocess_args(args(&[
        "print",
        "-nt",
        "prompt",
        "-xt",
        "ask_question",
        "-nc",
        "--name",
        "session",
    ]));
    assert_eq!(
        cleaned,
        args(&[
            "print",
            "--no-tools",
            "prompt",
            "--exclude-tools",
            "ask_question",
            "--no-context-files",
            "--name",
            "session"
        ])
    );
    assert!(unknown.is_empty());
}

#[test]
fn preprocess_collects_unknown_flags_with_values() {
    let (cleaned, unknown) = pir::coding_agent::cli::preprocess_args(args(&[
        "--plan",
        "pro",
        "--verbose",
        "--other=1",
        "-e",
        "x.ts",
    ]));
    assert_eq!(cleaned, args(&["--verbose", "-e", "x.ts"]));
    assert_eq!(unknown.get("plan").map(String::as_str), Some("pro"));
    assert_eq!(unknown.get("other").map(String::as_str), Some("1"));
}

#[test]
fn preprocess_unknown_boolean_flag_consumes_value_like_upstream() {
    // Upstream attaches the next non-flag argument as the value.
    let (cleaned, unknown) =
        pir::coding_agent::cli::preprocess_args(args(&["--flagless", "prompt"]));
    assert_eq!(cleaned, args(&[]));
    assert_eq!(unknown.get("flagless").map(String::as_str), Some("prompt"));
    // A trailing flag stays boolean.
    let (_, unknown) = pir::coding_agent::cli::preprocess_args(args(&["--flagless", "--verbose"]));
    assert_eq!(unknown.get("flagless").map(String::as_str), Some("true"));
}

#[test]
fn preprocess_stops_collecting_at_double_dash() {
    let (cleaned, unknown) = pir::coding_agent::cli::preprocess_args(args(&["--", "--plan", "x"]));
    assert_eq!(cleaned, args(&["--", "--plan", "x"]));
    assert!(unknown.is_empty());
}

#[test]
fn comma_lists_split_and_trim() {
    assert_eq!(
        pir::coding_agent::cli::split_comma_list(Some(" read , bash,,edit")),
        vec!["read", "bash", "edit"]
    );
    assert!(pir::coding_agent::cli::split_comma_list(None).is_empty());
}

// ---------------------------------------------------------------------------
// P2-6: an invalid --mode is a hard error (upstream exits 1).
// ---------------------------------------------------------------------------

#[test]
fn invalid_mode_errors_and_exits_1() {
    for argv in [
        vec!["print", "hello", "--mode", "bogus"],
        vec!["--mode=bogus", "version"],
        vec!["--mode", "bogus", "version"],
    ] {
        let output = run(&argv);
        assert_eq!(
            output.status.code(),
            Some(1),
            "invalid --mode must exit 1: {argv:?}"
        );
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            text.contains("Invalid mode"),
            "expected the upstream invalid-mode error: {text}"
        );
    }

    // Valid modes still pass the mode gate. `--mode` explicitly selects the
    // print host, so the trailing word is a PROMPT, not the `version` command
    // (PIR-BIN-030; upstream `resolveAppMode` decides the host from flags and
    // then treats every positional as a message). The hermetic agent dir has
    // no provider, so the prompt run fails on the credential — but never with
    // the mode error, and never with the command's version banner.
    for argv in [
        vec!["--mode", "text", "version"],
        vec!["--mode=json", "version"],
    ] {
        let output = run(&argv);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("Invalid mode"),
            "valid --mode rejected: {argv:?}: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("Rust edition:"),
            "`{argv:?}` must treat `version` as a prompt, not run the command: {stdout}"
        );
    }
}

// ---------------------------------------------------------------------------
// P2-7: argv conversion is lossy, never panicking on non-UTF-8.
// ---------------------------------------------------------------------------

#[test]
fn lossy_arg_replaces_invalid_utf8() {
    assert_eq!(pir::coding_agent::cli::lossy_arg("plain"), "plain");

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        // An unpaired surrogate is invalid UTF-16: lossy conversion yields
        // U+FFFD instead of panicking (env::args would panic here).
        let invalid = std::ffi::OsString::from_wide(&[0xD800]);
        assert_eq!(pir::coding_agent::cli::lossy_arg(&invalid), "\u{FFFD}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let invalid = std::ffi::OsString::from_vec(vec![0xFF]);
        assert_eq!(pir::coding_agent::cli::lossy_arg(&invalid), "\u{FFFD}");
    }
}

// ---------------------------------------------------------------------------
// Audit §2 #16: extension-registered CLI flags
//
// `--help` is clap's and exits before any extension runtime exists, so it can
// never execute extension factories (upstream's help does, which is exactly
// the side effect pir refuses to copy). `--help-extensions` is the explicit
// opt-in that loads the plan and prints the upstream `Extension CLI Flags:`
// section.
// ---------------------------------------------------------------------------

struct ExtensionHelpFixture {
    root: std::path::PathBuf,
    project: std::path::PathBuf,
    agent: std::path::PathBuf,
}

impl ExtensionHelpFixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pir-cli-extension-help-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("project");
        let agent = root.join("agent");
        std::fs::create_dir_all(project.join(".pi").join("extensions")).expect("project dirs");
        std::fs::create_dir_all(agent.join("extensions")).expect("agent dirs");
        Self {
            root,
            project,
            agent,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], extra_env: &[(&str, &str)]) -> std::process::Output {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
        command
            .current_dir(&self.project)
            .args(args)
            .env("PIR_AGENT_DIR", &self.agent)
            .env("PI_CODING_AGENT_DIR", &self.agent)
            // Hermetic home: no user `.agents/skills`, no real trust store.
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env_remove("PIR_TRUST_PROJECT");
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().expect("pir binary should start")
    }

    fn project_extension(&self, flag: &str) -> String {
        let path = self
            .project
            .join(".pi")
            .join("extensions")
            .join("project-ext.js");
        std::fs::write(
            &path,
            format!(
                "export default function (pi) {{ pi.registerFlag({flag:?}, {{ type: \"string\", description: \"project flag\" }}); }}\n"
            ),
        )
        .expect("write project extension");
        path.display().to_string()
    }

    fn trust_extension(&self, body: &str) -> String {
        let path = self.root.join("trust-ext.js");
        std::fs::write(
            &path,
            format!("export default function (pi) {{ {body} }}\n"),
        )
        .expect("write trust extension");
        path.display().to_string()
    }

    /// A standalone extension (in the hermetic home, so `-e ~/…` resolves to
    /// it) that registers one string CLI flag.
    fn string_flag_extension(&self, flag: &str) -> String {
        let path = self.root.join(format!("{flag}-ext.js"));
        std::fs::write(
            &path,
            format!(
                "export default function (pi) {{ pi.registerFlag({flag:?}, {{ type: \"string\", description: \"test flag\" }}); }}\n"
            ),
        )
        .expect("write flag extension");
        path.display().to_string()
    }
}

#[test]
fn help_never_executes_extension_factories_but_help_extensions_does() {
    let fixture = ExtensionHelpFixture::new("side-effect");
    let sentinel = fixture.root.join("factory-ran.txt");
    let sentinel_js = format!("{:?}", sentinel.display().to_string());
    std::fs::write(
        fixture.agent.join("extensions").join("side-effect.js"),
        format!(
            "import fs from \"node:fs\";\n\
             export default function (pi) {{\n\
               fs.writeFileSync({sentinel_js}, \"loaded\");\n\
               pi.registerFlag(\"side-effect-flag\", {{ type: \"boolean\", description: \"loaded flag\" }});\n\
             }}\n"
        ),
    )
    .expect("write side-effect extension");

    for help_flag in ["--help", "-h"] {
        let output = fixture.run(&[help_flag]);
        assert!(
            output.status.success(),
            "pir {help_flag} must exit 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !sentinel.exists(),
            "pir {help_flag} must not execute an extension factory"
        );
        // The static help points at the dynamic surface without loading it.
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("--help-extensions"),
            "pir {help_flag} must mention --help-extensions: {text}"
        );
    }

    let output = fixture.run(&["--help-extensions"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        sentinel.exists(),
        "--help-extensions must load the extensions"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("Extension CLI Flags:"),
        "missing the upstream section header: {text}"
    );
    assert!(
        text.contains("--side-effect-flag"),
        "the registered flag must be listed: {text}"
    );
    assert!(
        text.contains("loaded flag"),
        "the registered description must be listed: {text}"
    );
}

#[test]
fn help_extensions_applies_an_extension_project_trust_grant_and_remember() {
    let fixture = ExtensionHelpFixture::new("grant");
    fixture.project_extension("project-flag");
    let grant = fixture
        .trust_extension("pi.on(\"project_trust\", () => ({ trusted: \"yes\", remember: true }));");

    let output = fixture.run(&["--help-extensions", "-e", &grant]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--project-flag <value>"),
        "the grant must open the project extension scope: {text}"
    );
    // `remember: true` persisted the run's decision.
    let store = pir::config::trust::ProjectTrustStore::new(&fixture.agent);
    assert_eq!(
        store.get(&fixture.project).map(|entry| entry.decision),
        Some(true),
        "remember:true must persist the extension grant"
    );
}

/// The startup's explicit `--no-approve` wins over an extension grant, and a
/// later load's `remember: true` cannot persist the rejected grant.
#[test]
fn help_extensions_cli_override_beats_a_remembering_grant() {
    let fixture = ExtensionHelpFixture::new("override");
    fixture.project_extension("project-flag");
    let grant = fixture
        .trust_extension("pi.on(\"project_trust\", () => ({ trusted: \"yes\", remember: true }));");

    let output = fixture.run(&["--help-extensions", "--no-approve", "-e", &grant]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.contains("--project-flag"),
        "--no-approve must keep project extensions unloaded: {text}"
    );
    assert_eq!(
        pir::config::trust::ProjectTrustStore::new(&fixture.agent)
            .get(&fixture.project)
            .map(|entry| entry.decision),
        None,
        "a rejected grant must not be persisted by the final load"
    );
}

#[test]
fn help_extensions_denial_beats_the_store_and_no_extension_falls_back_to_it() {
    let fixture = ExtensionHelpFixture::new("deny");
    fixture.project_extension("project-flag");
    // A stored "trusted" decision on disk.
    let store = pir::config::trust::ProjectTrustStore::new(&fixture.agent);
    store
        .set(&fixture.project, Some(true))
        .expect("seed the trust store");

    let deny = fixture.trust_extension("pi.on(\"project_trust\", () => ({ trusted: \"no\" }));");
    let denied = fixture.run(&["--help-extensions", "-e", &deny]);
    assert!(
        denied.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let denied_text = String::from_utf8_lossy(&denied.stdout);
    assert!(
        !denied_text.contains("--project-flag"),
        "an extension denial must keep project extensions unloaded: {denied_text}"
    );
    // A denial without `remember` leaves the stored decision alone.
    assert_eq!(
        store.get(&fixture.project).map(|entry| entry.decision),
        Some(true),
        "remember:false must not rewrite the store"
    );

    // No extension decision at all: the stored yes decides (the fallback
    // order env > extension > store/default).
    let fallback = fixture.run(&["--help-extensions"]);
    assert!(
        fallback.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    let fallback_text = String::from_utf8_lossy(&fallback.stdout);
    assert!(
        fallback_text.contains("--project-flag <value>"),
        "without an extension decision the store must grant trust: {fallback_text}"
    );
}

// ---------------------------------------------------------------------------
// Audit §5.2 #1: unregistered unknown long options are a hard error
//
// Upstream `applyExtensionFlagValues` (agent-session-services.ts:82-128)
// reports `Unknown option: --x` / `Extension flag "--name" requires a value`
// as error diagnostics; main.ts:918-926 exits 1 when any are present, while
// `--help` / `--list-models` report them and still exit 0.
// ---------------------------------------------------------------------------

#[test]
fn unregistered_long_option_exits_1() {
    let fixture = ExtensionHelpFixture::new("unknown-option");
    let output = fixture.run(&["--definitely-not-registered"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unregistered --flag must exit 1; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown option: --definitely-not-registered"),
        "expected the upstream wording: {stderr}"
    );
}

#[test]
fn multiple_unregistered_long_options_are_reported_together() {
    let fixture = ExtensionHelpFixture::new("unknown-options");
    let output = fixture.run(&["--alpha-flag", "--beta-flag"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown options: --alpha-flag, --beta-flag"),
        "expected one aggregated line: {stderr}"
    );
}

#[test]
fn string_extension_flag_without_a_value_exits_1() {
    let fixture = ExtensionHelpFixture::new("flag-missing-value");
    let extension = fixture.string_flag_extension("needs-value");
    let output = fixture.run(&["-e", &extension, "--needs-value"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a bare string flag must exit 1; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Extension flag \"--needs-value\" requires a value"),
        "expected the upstream wording: {stderr}"
    );
}

#[test]
fn registered_string_flag_with_a_value_is_not_an_error() {
    let fixture = ExtensionHelpFixture::new("flag-with-value");
    let extension = fixture.string_flag_extension("needs-value");
    let output = fixture.run(&["--help-extensions", "-e", &extension, "--needs-value=hello"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Unknown option") && !stderr.contains("requires a value"),
        "a registered flag with a value must not be diagnosed: {stderr}"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("--needs-value <value>"), "got: {text}");

    // An explicit empty string (`--name=`) is a VALUE, not the missing-value
    // sentinel (upstream stores `""`, not boolean `true`).
    let output = fixture.run(&["--help-extensions", "-e", &extension, "--needs-value="]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("requires a value"),
        "--flag= must be accepted"
    );
}

#[test]
fn help_stays_a_normal_exit_with_an_unregistered_option() {
    // `--help` is clap's and exits before any extension runtime exists (the
    // design pinned by `help_never_executes_extension_factories_but_help_extensions_does`),
    // so the new validation must never turn it into an error.
    let fixture = ExtensionHelpFixture::new("help-unknown-option");
    let output = fixture.run(&["--help", "--definitely-not-registered"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Usage:"), "help output missing: {text}");
}

#[test]
fn help_extensions_reports_unknown_options_but_exits_0() {
    // Upstream main.ts:881-888: `--help` reports startup diagnostics, prints
    // the help, and exits 0; the pir opt-in equivalent is --help-extensions.
    let fixture = ExtensionHelpFixture::new("help-extensions-unknown");
    let output = fixture.run(&["--help-extensions", "--definitely-not-registered"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown option: --definitely-not-registered"),
        "the diagnostic must be printed before help: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Audit §5.2 #7: CLI path flags go through the upstream normalize/resolve rules
// (`~`, `~\`, `file://`, relative-to-cwd). Without this a literal `~/x`
// became a directory named `~` under the cwd.
// ---------------------------------------------------------------------------

#[test]
fn session_dir_flag_expands_tilde() {
    let fixture = ExtensionHelpFixture::new("session-dir-tilde");
    let output = fixture.run(&["session", "--session-dir", "~/sessions"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fixture.root.join("sessions");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!("Session directory: {}", expected.display())),
        "expected {} in: {text}",
        expected.display()
    );
}

#[cfg(windows)]
#[test]
fn session_dir_flag_expands_windows_tilde() {
    let fixture = ExtensionHelpFixture::new("session-dir-tilde-win");
    let output = fixture.run(&["session", "--session-dir", "~\\sessions-win"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fixture.root.join("sessions-win");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!("Session directory: {}", expected.display())),
        "expected {} in: {text}",
        expected.display()
    );
}

#[test]
fn session_dir_env_expands_tilde() {
    let fixture = ExtensionHelpFixture::new("session-dir-env");
    let output = fixture.run_with_env(
        &["session"],
        &[("PI_CODING_AGENT_SESSION_DIR", "~/env-sessions")],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fixture.root.join("env-sessions");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!("Session directory: {}", expected.display())),
        "expected {} in: {text}",
        expected.display()
    );
}

#[test]
fn session_dir_setting_expands_tilde() {
    let fixture = ExtensionHelpFixture::new("session-dir-setting");
    std::fs::write(
        fixture.agent.join("settings.json"),
        r#"{"sessionDir":"~/settings-sessions"}"#,
    )
    .expect("write settings.json");
    let output = fixture.run(&["session"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fixture.root.join("settings-sessions");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!("Session directory: {}", expected.display())),
        "expected {} in: {text}",
        expected.display()
    );
}

#[test]
fn session_dir_relative_value_resolves_against_cwd() {
    let fixture = ExtensionHelpFixture::new("session-dir-relative");
    let output = fixture.run(&["session", "--session-dir", "relative-sessions"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fixture.project.join("relative-sessions");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!("Session directory: {}", expected.display())),
        "expected {} in: {text}",
        expected.display()
    );
}

#[test]
fn extension_path_expands_tilde() {
    let fixture = ExtensionHelpFixture::new("extension-tilde");
    let extension = fixture.string_flag_extension("tilde-flag");
    let file_name = std::path::Path::new(&extension)
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    let output = fixture.run(&["--help-extensions", "-e", &format!("~/{file_name}")]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--tilde-flag <value>"),
        "the -e ~/… path must load the extension: {text}"
    );
}

#[cfg(windows)]
#[test]
fn extension_path_expands_windows_tilde() {
    let fixture = ExtensionHelpFixture::new("extension-tilde-win");
    let extension = fixture.string_flag_extension("tilde-win-flag");
    let file_name = std::path::Path::new(&extension)
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    let output = fixture.run(&["--help-extensions", "-e", &format!("~\\{file_name}")]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--tilde-win-flag <value>"),
        "the -e ~\\… path must load the extension: {text}"
    );
}

#[test]
fn extension_path_accepts_file_url() {
    let fixture = ExtensionHelpFixture::new("extension-file-url");
    let extension = fixture.string_flag_extension("file-url-flag");
    #[cfg(windows)]
    let url = format!("file:///{}", extension.replace('\\', "/"));
    #[cfg(not(windows))]
    let url = format!("file://{extension}");
    let output = fixture.run(&["--help-extensions", "-e", &url]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--file-url-flag <value>"),
        "the file:// -e path must load the extension: {text}"
    );
}

// ---------------------------------------------------------------------------
// PIR-BIN-080: Windows path identity for the session cwd.
// ---------------------------------------------------------------------------

/// A Windows path names the same directory regardless of case (`c:\Proj` and
/// `C:\proj`), but the session header records the spelling verbatim. These
/// tests drive the REAL binary against a loopback mock provider and seed a
/// session from the real-cased directory, then run every session route from
/// the same directory spelled with a lowercase drive. Each route must open the
/// seeded session (one JSONL file, prior transcript re-sent) instead of
/// starting/forking a new one.
#[cfg(windows)]
mod windows_session_cwd_case {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    enum Step {
        Tool(&'static str, &'static str),
        Text(&'static str),
    }

    fn sse_chunk(delta: serde_json::Value, finish: serde_json::Value) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
        )
    }

    /// Serve `script` steps in order (the last repeats) over SSE and record
    /// every request body.
    fn spawn_mock(script: Vec<Step>) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("mock addr").port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&bodies);
        std::thread::spawn(move || {
            let mut cursor = 0usize;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = Vec::new();
                let mut buf = [0u8; 8192];
                let header_end = loop {
                    if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break None,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                let len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < header_end + len {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let body = String::from_utf8_lossy(&request[header_end..]).to_string();
                recorded.lock().unwrap().push(body);

                let step = script
                    .get(cursor.min(script.len().saturating_sub(1)))
                    .cloned();
                cursor += 1;
                let events = match step {
                    Some(Step::Tool(name, arguments)) => {
                        let mut events = sse_chunk(
                            serde_json::json!({"role": "assistant", "content": ""}),
                            serde_json::Value::Null,
                        );
                        events.push_str(&sse_chunk(
                            serde_json::json!({
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "call_mock_0",
                                    "type": "function",
                                    "function": {"name": name, "arguments": arguments},
                                }],
                            }),
                            serde_json::Value::Null,
                        ));
                        let mut final_chunk: serde_json::Value = serde_json::from_str(
                            sse_chunk(serde_json::json!({}), serde_json::json!("tool_calls"))
                                .trim_start_matches("data: ")
                                .trim_end(),
                        )
                        .unwrap();
                        final_chunk["usage"] = serde_json::json!({
                            "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                        });
                        events.push_str(&format!("data: {final_chunk}\n\n"));
                        events.push_str("data: [DONE]\n\n");
                        events
                    }
                    Some(Step::Text(text)) => {
                        let mut events = sse_chunk(
                            serde_json::json!({"role": "assistant", "content": ""}),
                            serde_json::Value::Null,
                        );
                        events.push_str(&sse_chunk(
                            serde_json::json!({"content": text}),
                            serde_json::Value::Null,
                        ));
                        let mut final_chunk: serde_json::Value = serde_json::from_str(
                            sse_chunk(serde_json::json!({}), serde_json::json!("stop"))
                                .trim_start_matches("data: ")
                                .trim_end(),
                        )
                        .unwrap();
                        final_chunk["usage"] = serde_json::json!({
                            "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                        });
                        events.push_str(&format!("data: {final_chunk}\n\n"));
                        events.push_str("data: [DONE]\n\n");
                        events
                    }
                    None => "data: [DONE]\n\n".to_owned(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    events.len(),
                    events
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (port, bodies)
    }

    fn write_agent(agent: &Path, port: u16) {
        std::fs::create_dir_all(agent).expect("create agent dir");
        std::fs::write(
            agent.join("models.json"),
            format!(
                r#"{{"providers":{{"mock":{{"name":"Mock","baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"k","models":[{{"id":"mock-model","name":"M","api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","reasoning":false,"input":["text"],"contextWindow":100000,"maxTokens":1024}}]}}}}}}"#
            ),
        )
        .expect("write models.json");
        std::fs::write(
            agent.join("settings.json"),
            r#"{"defaultProjectTrust":"always","quietStartup":true}"#,
        )
        .expect("write settings.json");
    }

    fn run_case(cwd: &Path, agent: &Path, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_pir"))
            .current_dir(cwd)
            .env("PIR_AGENT_DIR", agent)
            .env("PIR_CODING_AGENT_DIR", agent)
            .env("PI_CODING_AGENT_DIR", agent)
            .env("PI_OFFLINE", "1")
            .env("PIR_NO_UPDATE_CHECK", "1")
            .env("PIR_SKIP_VERSION_CHECK", "1")
            .env("PIR_TRUST_PROJECT", "1")
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .expect("pir binary should start")
    }

    fn base_args() -> Vec<&'static str> {
        vec![
            "print",
            "--mode",
            "json",
            "--no-context-files",
            "--offline",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--provider",
            "mock",
            "--model",
            "mock-model",
        ]
    }

    fn jsonl_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    fn header(path: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(path).expect("read session file");
        serde_json::from_str(text.lines().next().expect("session header line"))
            .expect("session header JSON")
    }

    fn request_roles(bodies: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        let bodies = bodies.lock().unwrap();
        let Some(body) = bodies.last() else {
            return Vec::new();
        };
        let value: serde_json::Value =
            serde_json::from_str(body).expect("request body must be JSON");
        value["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| {
                        message
                            .get("role")
                            .and_then(|role| role.as_str())
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn unique_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pir-cli-cwd-case-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create case root");
        root
    }

    /// Seed one session from `real` and return `(session_dir, session_id)`.
    fn seed_session(agent: &Path, real: &Path) -> (PathBuf, String) {
        let (port, _bodies) = spawn_mock(vec![
            Step::Tool("read", r#"{"path":"seed.txt"}"#),
            Step::Text("seed-done"),
        ]);
        write_agent(agent, port);
        let mut args = base_args();
        args.extend(["--tools", "read", "seed"]);
        let output = run_case(real, agent, &args);
        assert!(
            output.status.success(),
            "seed run failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let files = jsonl_files(agent);
        assert_eq!(files.len(), 1, "seed must persist exactly one session file");
        let header_value = header(&files[0]);
        let id = header_value["id"].as_str().expect("session id").to_owned();
        let dir = files[0].parent().expect("session dir").to_path_buf();
        (dir, id)
    }

    #[test]
    fn windows_session_cwd_case_variants_open_the_existing_session() {
        let root = unique_root("routes");
        let real = root.join("proj").join("RealDir");
        let other = root.join("proj").join("OtherDir");
        std::fs::create_dir_all(&real).expect("create real dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        std::fs::write(real.join("seed.txt"), "CMD-SEED\n").expect("seed file");
        // Same directory, lowercase spelling (what `cd /d c:\...` or a pasted
        // path produces). NTFS resolves it to the real directory.
        let lower = PathBuf::from(real.to_string_lossy().to_lowercase());
        assert_ne!(
            real.to_string_lossy(),
            lower.to_string_lossy(),
            "fixture needs a path with at least one uppercase character"
        );

        let routes: Vec<(&str, Vec<String>)> = vec![
            ("continue", vec!["--continue".to_owned()]),
            ("session", vec!["--session".to_owned(), String::new()]),
            ("resume", vec!["--resume".to_owned()]),
        ];
        for (route, mut route_args) in routes {
            let agent = root.join(format!("agent-{route}"));
            let (_session_dir, id) = seed_session(&agent, &real);
            if route == "session" {
                // Partial id, like the issue's `--session <partial>` repro.
                route_args[1] = id[..8].to_owned();
            }
            let (port, bodies) = spawn_mock(vec![Step::Text("route-done")]);
            write_agent(&agent, port);
            let mut args = base_args();
            args.extend(route_args.iter().map(String::as_str));
            args.push("go");
            let output = run_case(&lower, &agent, &args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "`--{route}` from the lowercase cwd failed: stderr={stderr}"
            );
            let files = jsonl_files(&agent);
            assert_eq!(
                files.len(),
                1,
                "`--{route}` from `{}` must open the seeded session, not create another \
                 (files: {files:?})",
                lower.display()
            );
            let roles = request_roles(&bodies);
            assert!(
                roles.iter().any(|role| role == "tool"),
                "`--{route}` must re-send the seeded transcript; roles={roles:?}"
            );
            assert!(
                !stderr.contains("different project"),
                "`--{route}` must not report the user's own directory as another project: \
                 {stderr}"
            );
            assert!(
                !stderr.contains("no sessions found"),
                "`--{route}` must find the project's session: {stderr}"
            );
        }

        // --- negative controls: a genuinely different directory ------------
        let agent = root.join("agent-controls");
        let (shared_dir, id) = seed_session(&agent, &real);
        let shared_dir_text = shared_dir.to_string_lossy().into_owned();
        let files_before = jsonl_files(&agent).len();

        // `--session <id>` from OtherDir with the SAME explicit session dir:
        // the id only matches a session of another project, so the plan stays
        // cross-project (notice + fork), never an in-place open.
        let (port, _bodies) = spawn_mock(vec![Step::Text("fork-done")]);
        write_agent(&agent, port);
        let mut args = base_args();
        args.extend(["--session", &id, "--session-dir", &shared_dir_text, "go"]);
        let output = run_case(&other, &agent, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "cross-project --session failed: stderr={stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "Session found in different project: {}",
                real.display()
            )),
            "a genuinely different directory must keep the cross-project notice: {stderr}"
        );
        let files_after_fork = jsonl_files(&agent).len();
        assert_eq!(
            files_after_fork,
            files_before + 1,
            "the cross-project route forks a NEW session in the shared dir"
        );

        // `--session <id>` from the case-variant cwd with the same explicit
        // session dir: the resolved-path comparison folds case, so this opens
        // the seeded session instead of forking.
        let (port, _bodies) = spawn_mock(vec![Step::Text("open-done")]);
        write_agent(&agent, port);
        let mut args = base_args();
        args.extend(["--session", &id, "--session-dir", &shared_dir_text, "go"]);
        let output = run_case(&lower, &agent, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "case-variant --session failed: stderr={stderr}"
        );
        assert!(
            !stderr.contains("different project"),
            "the same directory spelled with another case is not another project: {stderr}"
        );
        assert_eq!(
            jsonl_files(&agent).len(),
            files_after_fork,
            "opening the seeded session must not fork another file"
        );

        // `--continue` from OtherDir (default session dirs): the seed lives in
        // RealDir's cwd-encoded dir, so OtherDir starts its own session and
        // must NOT load the seed's transcript.
        let (port, bodies) = spawn_mock(vec![Step::Text("other-done")]);
        write_agent(&agent, port);
        let mut args = base_args();
        args.extend(["--continue", "go"]);
        let output = run_case(&other, &agent, &args);
        assert!(
            output.status.success(),
            "OtherDir --continue failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let roles = request_roles(&bodies);
        assert!(
            !roles.iter().any(|role| role == "tool"),
            "OtherDir --continue must not load RealDir's transcript; roles={roles:?}"
        );
        assert_eq!(
            jsonl_files(&agent).len(),
            files_after_fork + 1,
            "OtherDir --continue starts a session in its own cwd-encoded dir"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
