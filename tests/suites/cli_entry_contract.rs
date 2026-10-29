//! Upstream CLI entry contract (item 1 of the port stream).
//!
//! Mirrors `packages/coding-agent/src/cli/args.ts` (positional/`@file`
//! splitting, `--print`), `main.ts` `resolveAppMode` + `readPipedStdin` +
//! `buildInitialMessage`/`initial-message.ts`, and `cli/file-processor.ts`.
//!
//! Also covers the process-level CLI contract regressions: post-parse flag
//! validation (`--provider requires --model`, `--name`, `--session-id`), the
//! upstream exit codes (parse errors exit 1, `--help`/`-v` exit 0 with a bare
//! semver), the `--list-models` table, the `config` package/resource listing,
//! the package-management roundtrip, and the JSON-mode in-band provider
//! error (exit 0). Failed prompts run against the shared instant-401 sandbox
//! endpoint (`common::sandbox_endpoint()`), never an unreachable host.

#[path = "../common/mod.rs"]
pub(crate) mod common;

use std::io::Cursor;
use std::process::Command;

use clap::Parser;
use pir::coding_agent::cli::{
    build_print_prompts, preprocess_args, process_file_arguments, read_piped_input,
    resolve_app_mode, split_at_args, validate_parsed_flags, validate_rpc_file_args, AppMode, Cli,
    PrintPrompts, PIPED_STDIN_MAX_BYTES,
};

fn parse(argv: &[&str]) -> Cli {
    let mut full = vec!["pir"];
    full.extend_from_slice(argv);
    Cli::try_parse_from(full).expect("argv should parse")
}

// ---------------------------------------------------------------------------
// resolveAppMode (main.ts:112-123)
// ---------------------------------------------------------------------------

#[test]
fn resolve_app_mode_matches_upstream_precedence() {
    // `--mode rpc` / `--mode json` win outright.
    assert_eq!(
        resolve_app_mode(Some(pir::coding_agent::cli::Mode::Rpc), false, true, true),
        AppMode::Rpc
    );
    assert_eq!(
        resolve_app_mode(Some(pir::coding_agent::cli::Mode::Json), true, true, true),
        AppMode::Json
    );
    // Explicit `--print`.
    assert_eq!(resolve_app_mode(None, true, true, true), AppMode::Print);
    // Auto-print: either stream not a TTY (upstream `!stdinIsTTY || !stdoutIsTTY`).
    assert_eq!(resolve_app_mode(None, false, false, true), AppMode::Print);
    assert_eq!(resolve_app_mode(None, false, true, false), AppMode::Print);
    assert_eq!(resolve_app_mode(None, false, false, false), AppMode::Print);
    // Interactive only when both are TTYs and no print flag.
    assert_eq!(
        resolve_app_mode(None, false, true, true),
        AppMode::Interactive
    );
    // `--mode text` does not force print (upstream only special-cases rpc/json).
    assert_eq!(
        resolve_app_mode(Some(pir::coding_agent::cli::Mode::Text), false, true, true),
        AppMode::Interactive
    );
}

#[test]
fn resolve_app_mode_text_mode_with_non_tty_is_print() {
    assert_eq!(
        resolve_app_mode(Some(pir::coding_agent::cli::Mode::Text), false, false, true),
        AppMode::Print
    );
}

// ---------------------------------------------------------------------------
// Positional splitting (args.ts:84-89, 247-248)
// ---------------------------------------------------------------------------

#[test]
fn split_at_args_separates_file_references() {
    let (messages, files) = split_at_args(vec![
        "hello".to_string(),
        "@notes.md".to_string(),
        "world".to_string(),
        "@/abs/path.txt".to_string(),
    ]);
    assert_eq!(messages, vec!["hello", "world"]);
    assert_eq!(files, vec!["notes.md", "/abs/path.txt"]);
}

#[test]
fn split_at_args_keeps_a_bare_at_sign_as_a_message() {
    // Upstream `arg.startsWith("@")` pushes `arg.slice(1)` with no emptiness
    // check: a lone `@` (and a blank `@ `) are file references whose open
    // fails downstream, exactly like upstream.
    let (messages, files) = split_at_args(vec!["@".to_string(), "@ ".to_string()]);
    assert!(messages.is_empty());
    assert_eq!(files, vec!["", " "]);
}

#[test]
fn entry_plan_print_flag_and_positionals() {
    // `pir -p hello world` -> print, message list ["hello", "world"].
    let cli = parse(&["-p", "hello", "world"]);
    let plan = cli.entry_plan(true, true);
    assert_eq!(plan.app_mode, AppMode::Print);
    assert_eq!(plan.messages, vec!["hello", "world"]);
    assert!(plan.file_args.is_empty());

    // Bare `-p` with positionals still puts them in messages.
    let cli = parse(&["-p", "only"]);
    let plan = cli.entry_plan(true, true);
    assert_eq!(plan.app_mode, AppMode::Print);
    assert_eq!(plan.messages, vec!["only"]);
}

#[test]
fn entry_plan_bare_print_flag_has_no_message() {
    let cli = parse(&["-p"]);
    let plan = cli.entry_plan(true, true);
    assert_eq!(plan.app_mode, AppMode::Print);
    assert!(plan.messages.is_empty());
    assert!(plan.file_args.is_empty());
}

#[test]
fn preprocess_args_keeps_clap_defined_long_flags() {
    // `preprocess_args` collects unknown `--flag` forms as extension flags;
    // every long flag clap itself defines must pass through instead — a
    // missing `KNOWN_LONG_FLAGS` entry swallowed the flag AND its detached
    // value before clap ever saw them (e.g. `pir --print "msg"` dropped both
    // the flag and the message, and `server --unix` silently became TCP).
    for flag in [
        "--print",
        "--unix",
        "--local",
        "--all",
        "--extensions",
        "--force",
        "--timeout",
        "--credentials",
        "--no-refresh",
        "--min-expiry",
        "--agent-name",
    ] {
        let (cleaned, unknown) = preprocess_args(vec!["pir".to_string(), flag.to_string()]);
        assert_eq!(
            cleaned,
            vec!["pir", flag],
            "{flag} must reach clap untouched"
        );
        assert!(
            unknown.is_empty(),
            "{flag} was collected as unknown: {unknown:?}"
        );
    }
}

#[test]
fn preprocess_args_keeps_print_flag_with_its_message() {
    // The concrete regression: `pir --print "msg"` lost the flag and the
    // message (the value attached to the collected extension flag), so the
    // run degraded or errored instead of entering print mode.
    let (cleaned, unknown) = preprocess_args(vec![
        "pir".to_string(),
        "--print".to_string(),
        "msg".to_string(),
    ]);
    assert_eq!(cleaned, vec!["pir", "--print", "msg"]);
    assert!(
        unknown.is_empty(),
        "print must not be an extension flag: {unknown:?}"
    );

    let cli = parse(&["--print", "msg"]);
    assert!(cli.print.is_some(), "--print must survive parsing");
    let plan = cli.entry_plan(true, true);
    assert_eq!(plan.app_mode, AppMode::Print);
    assert_eq!(plan.messages, vec!["msg"]);
}

#[test]
fn entry_plan_separates_at_files_from_positionals() {
    let cli = parse(&["@a.md", "explain", "@b.md"]);
    let plan = cli.entry_plan(true, true);
    assert_eq!(plan.messages, vec!["explain"]);
    assert_eq!(plan.file_args, vec!["a.md", "b.md"]);
}

#[test]
fn entry_plan_auto_print_on_non_tty() {
    // No subcommand, no -p, but stdout is piped -> print.
    let cli = parse(&["summarize this"]);
    assert_eq!(cli.entry_plan(true, false).app_mode, AppMode::Print);
    assert_eq!(cli.entry_plan(false, true).app_mode, AppMode::Print);
    // Both TTY -> interactive.
    assert_eq!(cli.entry_plan(true, true).app_mode, AppMode::Interactive);
}

#[test]
fn entry_plan_json_mode_positionals() {
    let cli = parse(&["--mode", "json", "what is 2+2"]);
    let plan = cli.entry_plan(true, true);
    assert_eq!(plan.app_mode, AppMode::Json);
    assert_eq!(plan.messages, vec!["what is 2+2"]);
}

// ---------------------------------------------------------------------------
// Piped stdin (main.ts `readPipedStdin`)
// ---------------------------------------------------------------------------

#[test]
fn read_piped_input_is_none_for_a_tty() {
    let reader = Cursor::new(b"ignored".to_vec());
    assert_eq!(read_piped_input(true, reader), None);
}

#[test]
fn read_piped_input_trims_and_rejects_empty() {
    let reader = Cursor::new(b"  hello from a pipe \r\n".to_vec());
    assert_eq!(
        read_piped_input(false, reader).as_deref(),
        Some("hello from a pipe")
    );
    let reader = Cursor::new(b"   \n\t ".to_vec());
    assert_eq!(read_piped_input(false, reader), None);
    let reader = Cursor::new(Vec::new());
    assert_eq!(read_piped_input(false, reader), None);
}

#[test]
fn read_piped_input_is_bounded() {
    // A read larger than the cap must not be slurped whole: the content is
    // truncated to exactly PIPED_STDIN_MAX_BYTES.
    let big = vec![b'x'; (PIPED_STDIN_MAX_BYTES as usize) + 4096];
    let text = read_piped_input(false, Cursor::new(big)).expect("content");
    assert_eq!(text.len() as u64, PIPED_STDIN_MAX_BYTES);
}

// ---------------------------------------------------------------------------
// buildInitialMessage (cli/initial-message.ts)
// ---------------------------------------------------------------------------

#[test]
fn build_print_prompts_joins_stdin_file_and_first_message() {
    let prompts = build_print_prompts(
        Some("stdin\n"),
        "<file name=\"a.md\">\nbody\n</file>\n",
        &["explain".to_string(), "and more".to_string()],
    );
    assert_eq!(
        prompts,
        PrintPrompts {
            initial_message: Some(
                "stdin\n<file name=\"a.md\">\nbody\n</file>\nexplain".to_string()
            ),
            additional_messages: vec!["and more".to_string()],
        }
    );
}

#[test]
fn build_print_prompts_empty_parts_yield_no_initial_message() {
    assert_eq!(
        build_print_prompts(None, "", &[]),
        PrintPrompts {
            initial_message: None,
            additional_messages: Vec::new(),
        }
    );
    // A file-only prompt still produces an initial message.
    let prompts = build_print_prompts(None, "<file name=\"a\">x</file>", &[]);
    assert_eq!(
        prompts.initial_message.as_deref(),
        Some("<file name=\"a\">x</file>")
    );
}

#[test]
fn build_print_prompts_shifts_only_the_first_message() {
    let prompts = build_print_prompts(None, "", &["one".to_string()]);
    assert_eq!(prompts.initial_message.as_deref(), Some("one"));
    assert!(prompts.additional_messages.is_empty());

    let prompts = build_print_prompts(None, "", &["one".to_string(), "two".to_string()]);
    assert_eq!(prompts.initial_message.as_deref(), Some("one"));
    assert_eq!(prompts.additional_messages, vec!["two"]);
}

// ---------------------------------------------------------------------------
// Post-parse flag validation (main.ts:315-336, 469-474, 709-716)
// ---------------------------------------------------------------------------

#[test]
fn validate_provider_requires_model() {
    let cli = parse(&["--provider", "anthropic"]);
    let error = validate_parsed_flags(&cli).expect_err("--provider requires --model");
    assert_eq!(
        error,
        "--provider requires --model (for example: --provider <provider> --model <model>)"
    );
    // With --model the pair is accepted.
    let cli = parse(&["--provider", "anthropic", "--model", "claude"]);
    assert!(validate_parsed_flags(&cli).is_ok());
}

#[test]
fn validate_auth_provider_flag_is_exempt() {
    // `auth` subcommands carry their OWN --provider flag, so the run-mode
    // rule must not fire for them.
    let cli = parse(&["auth", "print-api-key", "--provider", "openai"]);
    assert!(
        validate_parsed_flags(&cli).is_ok(),
        "auth --provider must be exempt from the model rule"
    );
}

#[test]
fn validate_empty_name_rejected() {
    let cli = parse(&["--name", ""]);
    let error = validate_parsed_flags(&cli).expect_err("empty --name");
    assert_eq!(error, "--name requires a non-empty value");
    let cli = parse(&["--name", "  "]);
    assert_eq!(
        validate_parsed_flags(&cli).expect_err("whitespace --name"),
        "--name requires a non-empty value"
    );
    let cli = parse(&["--name", "Refactor auth"]);
    assert!(validate_parsed_flags(&cli).is_ok());
}

#[test]
fn validate_invalid_session_id_rejected() {
    let cli = parse(&["--session-id", "bad id!"]);
    let error = validate_parsed_flags(&cli).expect_err("invalid --session-id");
    assert!(
        error.contains("Session id"),
        "upstream assertValidSessionId message: {error}"
    );
    let cli = parse(&["--session-id", "valid-id.1"]);
    assert!(validate_parsed_flags(&cli).is_ok());
}

// ---------------------------------------------------------------------------
// RPC + @file hard error (main.ts:659-662)
// ---------------------------------------------------------------------------

#[test]
fn rpc_with_file_args_is_a_hard_error() {
    let error =
        validate_rpc_file_args(AppMode::Rpc, &["notes.md".to_string()]).expect_err("must reject");
    assert_eq!(error, "@file arguments are not supported in RPC mode");
    // Every other mode accepts them.
    assert!(validate_rpc_file_args(AppMode::Rpc, &[]).is_ok());
    assert!(validate_rpc_file_args(AppMode::Print, &["notes.md".to_string()]).is_ok());
    assert!(validate_rpc_file_args(AppMode::Interactive, &["notes.md".to_string()]).is_ok());
}

#[test]
fn entry_plan_rpc_mode_is_detected_for_the_file_gate() {
    let cli = parse(&["--mode", "rpc", "@notes.md"]);
    let plan = cli.entry_plan(false, false);
    assert_eq!(plan.app_mode, AppMode::Rpc);
    assert_eq!(plan.file_args, vec!["notes.md"]);
    assert!(validate_rpc_file_args(plan.app_mode, &plan.file_args).is_err());
}

// ---------------------------------------------------------------------------
// Audit §5.2 #6: `--models` presence replaces settings.enabledModels
// (upstream main.ts:813 `parsed.models ?? settingsManager.getEnabledModels()`)
// ---------------------------------------------------------------------------

#[test]
fn models_patterns_separates_absence_from_an_explicit_empty_list() {
    let cli = parse(&["--models", " a , b,,c "]);
    assert_eq!(
        cli.models_patterns(),
        Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
    );

    // `--models ""` is PRESENT with no patterns: upstream parses it to `[]`
    // and that empty list replaces the settings scope.
    let cli = parse(&["--models", ""]);
    assert_eq!(cli.models_patterns(), Some(Vec::new()));

    // Absent: the settings list applies.
    let cli = parse(&["print", "hello"]);
    assert_eq!(cli.models_patterns(), None);
}

#[test]
fn effective_model_patterns_replaces_the_settings_scope() {
    use pir::coding_agent::overrides::effective_model_patterns;

    let settings = vec!["settings-a".to_string(), "settings-b".to_string()];
    // Flag absent -> settings.
    assert_eq!(
        effective_model_patterns(None, Some(&settings)),
        settings,
        "an absent --models must fall back to enabledModels"
    );
    // Flag present -> replaces, never merges.
    let cli = vec!["cli-a".to_string()];
    assert_eq!(effective_model_patterns(Some(&cli), Some(&settings)), cli);
    // Explicit empty -> empty scope (no settings fallback).
    assert_eq!(
        effective_model_patterns(Some(&[]), Some(&settings)),
        Vec::<String>::new(),
        "--models \"\" must clear the scope"
    );
    // Neither present.
    assert_eq!(effective_model_patterns(None, None), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// Audit §5.2 #7: CLI path resolution (`~`, `file://`, relative-to-cwd)
// ---------------------------------------------------------------------------

#[test]
fn resolve_cli_path_expands_relative_paths_and_file_urls() {
    use pir::coding_agent::cli::{is_local_cli_path, resolve_cli_path};

    let base = std::env::temp_dir();
    assert_eq!(
        resolve_cli_path("relative/sub", &base),
        base.join("relative").join("sub"),
        "a relative CLI path resolves against the process cwd"
    );

    let target = base.join("file-url-target");
    #[cfg(windows)]
    let url = format!("file:///{}", target.to_string_lossy().replace('\\', "/"));
    #[cfg(not(windows))]
    let url = format!("file://{}", target.to_string_lossy());
    assert_eq!(
        resolve_cli_path(&url, &base),
        target,
        "a file:// URL converts to the filesystem path (fileURLToPath)"
    );

    // Package sources / builtin names are not local paths and stay verbatim.
    for value in [
        "builtin:mcp",
        "npm:some-package",
        "git:github.com/x/y",
        "https://example.com/x.ts",
    ] {
        assert!(!is_local_cli_path(value), "{value} must not be local");
    }
    assert!(is_local_cli_path("./ext.ts"));
    assert!(is_local_cli_path("file:///tmp/ext.ts"));
}

// ---------------------------------------------------------------------------
// Process-level CLI contract (the `pir` binary): exit codes, --version,
// --list-models, config, package management, and the JSON-mode in-band error.
// ---------------------------------------------------------------------------

fn run(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pir"));
    cmd.args(args);
    cmd.output().expect("pir binary should start")
}

/// Sandboxed subprocess (isolated agent dir, no provider credentials).
fn sandbox_command(args: &[&str], label: &str) -> (std::process::Command, std::path::PathBuf) {
    let home = std::env::temp_dir().join(format!(
        "pir-cli-entry-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let agent_dir = home.join("agent");
    let _ = std::fs::create_dir_all(&agent_dir);
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_AGENT_DIR", &agent_dir);
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    (command, home)
}

/// Minimal models.json pointed at the shared instant-401 sandbox endpoint:
/// provider requests fail FAST with a non-retryable auth error.
fn sandbox_models_json() -> String {
    let base_url = common::sandbox_endpoint();
    format!(
        r#"{{"providers":{{"bench":{{"name":"rpc test","baseUrl":"{base_url}","apiKey":"x","api":"openai-completions","models":[{{"id":"bench-model","name":"bench","api":"openai-completions","baseUrl":"{base_url}","reasoning":false,"contextWindow":4096,"maxTokens":256,"input":["text"]}}]}}}}}}"#
    )
}

#[test]
fn version_flag_prints_only_the_semver() {
    // Upstream `-v`/`--version` prints ONLY the VERSION string (main.ts:634-637);
    // the richer output stays on the `pir version` subcommand.
    for argv in [vec!["-v"], vec!["--version"]] {
        let output = run(&argv);
        assert_eq!(output.status.code(), Some(0), "{argv:?} must exit 0");
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(text, env!("CARGO_PKG_VERSION"), "{argv:?} printed: {text}");
    }
}

#[test]
fn unknown_short_flag_exits_1() {
    // Upstream args.ts: `Unknown option: -x` -> exit 1 (clap's exit 2 is not
    // the upstream contract).
    let output = run(&["-x"]);
    assert_eq!(output.status.code(), Some(1), "unknown flag must exit 1");
    assert!(!String::from_utf8_lossy(&output.stderr).is_empty());
}

#[test]
fn help_still_exits_0() {
    let output = run(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
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
        "install",
        "remove",
        "uninstall",
        "update",
        "list",
    ] {
        assert!(text.contains(command), "help omitted {command}: {text}");
    }
}

#[test]
fn provider_without_model_exits_1() {
    // Upstream main.ts:469-474.
    let output = run(&["--provider", "anthropic", "version"]);
    assert_eq!(output.status.code(), Some(1), "got: {output:?}");
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("--provider requires --model"),
        "expected the upstream provider-without-model error: {text}"
    );
}

#[test]
fn auth_provider_flag_is_exempt_from_the_model_requirement() {
    // `auth` subcommands carry their OWN --provider flag: the run-mode rule
    // must not fire for them (the command may still fail on the unknown
    // provider, but never with the model-requirement error).
    let (mut command, _home) = sandbox_command(
        &["auth", "print-api-key", "--provider", "no-such-provider"],
        "auth-provider-exempt",
    );
    let output = command.output().expect("pir binary should start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("--provider requires --model"),
        "auth --provider must be exempt from the model rule: {stderr}"
    );
}

#[test]
fn empty_name_exits_1() {
    // Upstream main.ts:709-716.
    let output = run(&["--name", "", "version"]);
    assert_eq!(output.status.code(), Some(1), "got: {output:?}");
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("--name requires a non-empty value"),
        "expected the upstream empty-name error: {text}"
    );
}

#[test]
fn invalid_session_id_exits_1() {
    // Upstream main.ts:315-336 via assertValidSessionId.
    let output = run(&["--session-id", "bad id!", "version"]);
    assert_eq!(output.status.code(), Some(1), "got: {output:?}");
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("Session id"),
        "expected the session-id validation error: {text}"
    );
}

#[test]
fn config_lists_package_resources() {
    // `pir config` is the package/resource listing (upstream
    // handleConfigCommand), not the former debug dump. The listing names the
    // server auth token state but only ever prints it redacted
    // (production-safety guarantee).
    let output = run(&["config"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Resource configuration"), "got: {text}");
    assert!(text.contains("server_auth_token"), "got: {text}");
}

#[test]
fn list_models_prints_the_upstream_table() {
    let (mut command, home) = sandbox_command(&["--list-models"], "list-models");
    let _ = std::fs::write(
        home.join("agent").join("models.json"),
        sandbox_models_json(),
    );
    let output = command.output().expect("pir binary should start");
    assert!(output.status.success(), "got: {output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    // Padded header columns (upstream list-models.ts).
    for column in [
        "provider", "model", "context", "max-out", "thinking", "images",
    ] {
        assert!(
            text.contains(column),
            "table header omitted {column}: {text}"
        );
    }
    // The sandbox model appears as a row.
    assert!(text.contains("bench-model"), "row omitted: {text}");
    assert!(text.contains("4.1K"), "context formatted as 4.1K: {text}");
}

#[test]
fn list_models_search_miss_reports_no_match() {
    let (mut command, home) =
        sandbox_command(&["--list-models", "zzz-no-such"], "list-models-miss");
    let _ = std::fs::write(
        home.join("agent").join("models.json"),
        sandbox_models_json(),
    );
    let output = command.output().expect("pir binary should start");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("No models matching \"zzz-no-such\""),
        "got: {text}"
    );
}

#[test]
fn json_mode_provider_error_exits_0_with_header_and_error_event() {
    // Upstream print-mode.ts maps a terminal error onto the exit code only in
    // TEXT mode; `--mode json` exits 0 and reports the failure as an event
    // line. The run targets the instant-401 sandbox endpoint (fail fast); the
    // ephemeral (`--no-session`) run still opens with a synthesized session
    // header line, and every line stays standalone-parseable JSON.
    let (mut command, home) = sandbox_command(
        &["--mode", "json", "--no-session", "-p", "hi"],
        "json-provider-error",
    );
    let _ = std::fs::write(
        home.join("agent").join("models.json"),
        sandbox_models_json(),
    );
    let output = command.output().expect("pir binary should start");
    assert_eq!(
        output.status.code(),
        Some(0),
        "json mode reports provider errors in-band: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    assert!(
        first.contains("\"type\":\"session\""),
        "first json line must be the session header: {text}"
    );
    // The failure arrives in-band as an event line (the terminal message
    // carries the provider error).
    assert!(text.contains("error"), "missing error event: {text}");
    // Every line is standalone-parseable JSON.
    for line in text.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("json line {line:?} is not valid JSON: {error}"));
    }
}

#[test]
fn package_manager_local_roundtrip() {
    let home = std::env::temp_dir().join(format!(
        "pir-cli-entry-pkg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let agent_dir = home.join("agent");
    let _ = std::fs::create_dir_all(&agent_dir);
    let package = home.join("my-pkg");
    let _ = std::fs::create_dir_all(&package);
    let _ = std::fs::write(package.join("ext.js"), "export default {}");
    let package_text = package.to_string_lossy().into_owned();
    let env_home = home.clone();
    let env_agent = agent_dir.clone();

    let mut install = Command::new(env!("CARGO_BIN_EXE_pir"));
    install
        .args(["install"])
        .arg(&package_text)
        .env("HOME", &env_home)
        .env("USERPROFILE", &env_home)
        .env("PIR_AGENT_DIR", &env_agent);
    let output = install.output().expect("pir binary should start");
    assert!(output.status.success(), "install failed: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("Installed {package_text}")),
        "got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    // The source is recorded in the agent settings `packages` array.
    let settings = std::fs::read_to_string(agent_dir.join("settings.json")).expect("settings.json");
    assert!(settings.contains("packages"), "settings: {settings}");

    let mut list = Command::new(env!("CARGO_BIN_EXE_pir"));
    list.args(["list"])
        .env("HOME", &env_home)
        .env("USERPROFILE", &env_home)
        .env("PIR_AGENT_DIR", &env_agent);
    let output = list.output().expect("pir binary should start");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("User packages:"), "got: {text}");
    assert!(text.contains(&package_text), "got: {text}");

    let mut remove = Command::new(env!("CARGO_BIN_EXE_pir"));
    remove
        .args(["remove"])
        .arg(&package_text)
        .env("HOME", &env_home)
        .env("USERPROFILE", &env_home)
        .env("PIR_AGENT_DIR", &env_agent);
    let output = remove.output().expect("pir binary should start");
    assert!(output.status.success(), "remove failed: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("Removed {package_text}")),
        "got: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let mut list = Command::new(env!("CARGO_BIN_EXE_pir"));
    list.args(["list"])
        .env("HOME", &env_home)
        .env("USERPROFILE", &env_home)
        .env("PIR_AGENT_DIR", &env_agent);
    let output = list.output().expect("pir binary should start");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("No packages installed."),
        "got: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Removing an unknown source is the upstream exit-1 error.
    let mut remove = Command::new(env!("CARGO_BIN_EXE_pir"));
    remove
        .args(["remove", "does-not-exist"])
        .env("HOME", &env_home)
        .env("USERPROFILE", &env_home)
        .env("PIR_AGENT_DIR", &env_agent);
    let output = remove.output().expect("pir binary should start");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("No matching package found for does-not-exist"),
        "got: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// @file expansion (cli/file-processor.ts)
// ---------------------------------------------------------------------------

/// Per-test temp directory.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-cli-entry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write(&self, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn process_file_arguments_wraps_text_files() {
    let temp = TempDir::new("text");
    let path = temp.write("notes.md", b"line one\nline two\n");
    let text = process_file_arguments(&["notes.md".to_string()], &temp.0).expect("expand");
    let expected = format!(
        "<file name=\"{}\">\nline one\nline two\n\n</file>\n",
        path.display()
    );
    assert_eq!(text, expected);
}

#[test]
fn process_file_arguments_strips_a_leading_bom() {
    let temp = TempDir::new("bom");
    let path = temp.write("bom.txt", "\u{feff}body".as_bytes());
    let text = process_file_arguments(&["bom.txt".to_string()], &temp.0).expect("expand");
    assert_eq!(
        text,
        format!("<file name=\"{}\">\nbody\n</file>\n", path.display())
    );
    assert!(!text.contains('\u{feff}'));
}

#[test]
fn process_file_arguments_skips_empty_files() {
    let temp = TempDir::new("empty");
    temp.write("empty.txt", b"");
    let text = process_file_arguments(&["empty.txt".to_string()], &temp.0).expect("expand");
    assert_eq!(text, "");
}

#[test]
fn process_file_arguments_reports_missing_files() {
    let temp = TempDir::new("missing");
    let error = process_file_arguments(&["nope.txt".to_string()], &temp.0)
        .expect_err("missing files are hard errors (upstream exits 1)");
    let message = error.to_string();
    assert!(message.contains("File not found:"), "got: {message}");
    assert!(message.contains("nope.txt"), "got: {message}");
}

#[test]
fn process_file_arguments_accepts_absolute_paths_and_concatenates() {
    let temp = TempDir::new("absolute");
    let first = temp.write("a.txt", b"AAA");
    let second = temp.write("b.txt", b"BBB");
    let text = process_file_arguments(
        &[first.to_string_lossy().into_owned(), "b.txt".to_string()],
        &temp.0,
    )
    .expect("expand");
    let expected = format!(
        "<file name=\"{}\">\nAAA\n</file>\n<file name=\"{}\">\nBBB\n</file>\n",
        first.display(),
        second.display()
    );
    assert_eq!(text, expected);
}

#[test]
fn process_file_arguments_with_no_args_is_empty() {
    let temp = TempDir::new("none");
    assert_eq!(
        process_file_arguments(&[], &temp.0).expect("expand"),
        String::new()
    );
}
