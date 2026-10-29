//! Parity fixes for the feature re-scan (standalone test binary; the merged
//! `g_*` binaries and `suites/` are owned by another work stream):
//!
//! 1. ChatGPT/codex usage-limit friendly messages (upstream
//!    `openai-responses.ts:226-229` and `openai-codex-responses.ts`
//!    `parseErrorResponse`).
//! 2. Codex default transport retries = 0 (URL detection helper).
//! 3. `pir auth print-bearer-token --min-expiry` duration suffixes
//!    (`ms`/`s`/`m`/`h`, e.g. `30m` — upstream `cli.md:314`).
//! 4. `pir update --models/--extensions/--all/--force/--self/--extension`
//!    flags and their conditional conflicts (upstream
//!    `package-manager-cli.ts:442-580`), asserted at the clap parsing layer.
//!
//! 5. `pir mcp list` is its own upstream-shaped report, separate from
//!    `pir mcp check` (upstream `extensions/mcp/cli.ts:208-217,434-514`).

use clap::Parser;

use pir::ai::api::openai_responses::codex::{parse_error_response, CodexErrorResponse};
use pir::ai::transport::is_codex_responses_url;
use pir::ai::util::append_chatgpt_usage_hint;
use pir::coding_agent::cli::{parse_min_expiry, preprocess_args, Cli, UpdateRequest};

// ---------------------------------------------------------------------------
// 1a. ChatGPT usage-dashboard hint (openai-responses.ts)
// ---------------------------------------------------------------------------

#[test]
fn usage_hint_appends_dashboard_link_for_shared_limit_code() {
    let message = "OpenAI API error: HTTP 429 subscription_sharing_usage_limit_exceeded";
    assert_eq!(
        append_chatgpt_usage_hint(message),
        format!("{message}\nCheck your ChatGPT usage: https://chatgpt.com/settings/usage")
    );
}

#[test]
fn usage_hint_leaves_other_errors_untouched() {
    let message = "OpenAI API error: HTTP 500 internal error";
    assert_eq!(append_chatgpt_usage_hint(message), message);
    assert_eq!(append_chatgpt_usage_hint(""), "");
}

// ---------------------------------------------------------------------------
// 1b. Codex parseErrorResponse port
// ---------------------------------------------------------------------------

#[test]
fn codex_parse_error_response_builds_friendly_usage_limit_message() {
    // resets_at is Unix seconds; 30 minutes after `now`.
    let now_ms: u64 = 1_000_000_000_000;
    let resets_at = now_ms / 1000 + 30 * 60;
    let raw = format!(
        r#"{{"error":{{"code":"usage_limit_reached","message":"Weekly limit reached","plan_type":"Pro","resets_at":{resets_at}}}}}"#
    );
    let parsed = parse_error_response(&raw, 429, now_ms);
    assert_eq!(
        parsed,
        CodexErrorResponse {
            message: "Weekly limit reached".to_string(),
            friendly_message: Some(
                "You have hit your ChatGPT usage limit (pro plan). Try again in ~30 min."
                    .to_string()
            ),
        }
    );
}

#[test]
fn codex_parse_error_response_uses_friendly_message_when_error_message_missing() {
    let now_ms: u64 = 1_000_000_000_000;
    let raw = r#"{"error":{"type":"usage_not_included"}}"#;
    let parsed = parse_error_response(raw, 403, now_ms);
    assert_eq!(
        parsed,
        CodexErrorResponse {
            message: "You have hit your ChatGPT usage limit.".to_string(),
            friendly_message: Some("You have hit your ChatGPT usage limit.".to_string()),
        }
    );
}

#[test]
fn codex_parse_error_response_clamps_past_resets_to_zero_minutes() {
    let now_ms: u64 = 1_000_000_000_000;
    let raw = r#"{"error":{"code":"rate_limit_exceeded","resets_at":0}}"#;
    let parsed = parse_error_response(raw, 429, now_ms);
    assert_eq!(
        parsed.friendly_message.as_deref(),
        Some("You have hit your ChatGPT usage limit. Try again in ~0 min.")
    );
}

#[test]
fn codex_parse_error_response_passes_through_non_usage_errors() {
    let raw = r#"{"error":{"code":"invalid_request","message":"bad body"}}"#;
    let parsed = parse_error_response(raw, 400, 0);
    assert_eq!(
        parsed,
        CodexErrorResponse {
            message: "bad body".to_string(),
            friendly_message: None,
        }
    );
    // A non-JSON 429 carries no friendly message (upstream only inspects a
    // JSON `error` object; the raw body stays the message).
    let parsed = parse_error_response("gateway hiccup", 429, 0);
    assert_eq!(parsed.friendly_message, None);
    assert_eq!(parsed.message, "gateway hiccup");
    // Empty body degrades to the HTTP status line.
    let parsed = parse_error_response("", 500, 0);
    assert_eq!(parsed.message, "HTTP 500");
    assert_eq!(parsed.friendly_message, None);
}

// ---------------------------------------------------------------------------
// 2. Codex endpoint URL probe (default retries = 0)
// ---------------------------------------------------------------------------

#[test]
fn codex_responses_url_probe_matches_codex_endpoint_shapes() {
    assert!(is_codex_responses_url(
        "https://chatgpt.com/backend-api/codex/responses"
    ));
    assert!(is_codex_responses_url(
        "https://chatgpt.com/backend-api/codex/responses/"
    ));
    assert!(is_codex_responses_url(
        "https://chatgpt.com/backend-api/codex/responses?x=1"
    ));
    assert!(!is_codex_responses_url(
        "https://api.openai.com/v1/responses"
    ));
    assert!(!is_codex_responses_url(
        "https://chatgpt.com/backend-api/codex"
    ));
}

// ---------------------------------------------------------------------------
// 3. --min-expiry duration parsing
// ---------------------------------------------------------------------------

#[test]
fn min_expiry_accepts_bare_seconds_and_duration_suffixes() {
    assert_eq!(parse_min_expiry("30"), Ok(30));
    assert_eq!(parse_min_expiry("30s"), Ok(30));
    assert_eq!(parse_min_expiry("30m"), Ok(1_800));
    assert_eq!(parse_min_expiry("1h"), Ok(3_600));
    assert_eq!(parse_min_expiry("24h"), Ok(86_400));
    // Sub-second durations round up to one second (whole-second granularity).
    assert_eq!(parse_min_expiry("500ms"), Ok(1));
    assert_eq!(parse_min_expiry("0ms"), Ok(0));
}

#[test]
fn min_expiry_rejects_invalid_forms_with_usage_message() {
    for invalid in ["30x", "m", "-5s", "1.5h", "", "1 h"] {
        let error = parse_min_expiry(invalid).expect_err(invalid);
        assert!(
            error.contains("30m") && error.contains("ms, s, m, h"),
            "error should name the accepted forms: {error}"
        );
    }
}

#[test]
fn min_expiry_flag_parses_durations_through_clap() {
    let cli = Cli::try_parse_from([
        "pir",
        "auth",
        "print-bearer-token",
        "--provider",
        "openai",
        "--min-expiry",
        "30m",
    ])
    .expect("--min-expiry 30m should parse");
    match cli.command {
        Some(pir::coding_agent::cli::Command::Auth {
            command: Some(pir::coding_agent::cli::AuthCommand::PrintBearerToken { min_expiry, .. }),
        }) => assert_eq!(min_expiry, Some(1_800)),
        other => panic!("unexpected command: {other:?}"),
    }
    // Bare seconds keep working.
    let cli = Cli::try_parse_from([
        "pir",
        "auth",
        "print-bearer-token",
        "--provider",
        "openai",
        "--min-expiry",
        "90",
    ])
    .expect("--min-expiry 90 should parse");
    match cli.command {
        Some(pir::coding_agent::cli::Command::Auth {
            command: Some(pir::coding_agent::cli::AuthCommand::PrintBearerToken { min_expiry, .. }),
        }) => assert_eq!(min_expiry, Some(90)),
        other => panic!("unexpected command: {other:?}"),
    }
    // An invalid suffix is a clap error (exit-1 path).
    assert!(Cli::try_parse_from([
        "pir",
        "auth",
        "print-bearer-token",
        "--provider",
        "openai",
        "--min-expiry",
        "30w",
    ])
    .is_err());
}

// ---------------------------------------------------------------------------
// 4. pir update flags (clap layer + upstream conflict validation)
// ---------------------------------------------------------------------------

/// Parse through the binary's own preprocessing, so the upstream
/// `update --extension` spelling (rewritten to the internal long because the
/// global `-e/--extension` owns that long) behaves exactly as in the binary.
fn parse_preprocessed(argv: &[&str]) -> Cli {
    let (args, _) = preprocess_args(argv.iter().map(|arg| (*arg).to_string()).collect());
    let mut full = vec!["pir".to_string()];
    full.extend(args);
    Cli::try_parse_from(full).expect("preprocessed argv should parse")
}

fn update_request(argv: &[&str]) -> UpdateRequest {
    parse_preprocessed(argv)
        .update_request()
        .expect("update command")
}

fn update_plan(argv: &[&str]) -> pir::coding_agent::cli::UpdatePlan {
    parse_preprocessed(argv)
        .resolve_update_plan()
        .expect("update flags are valid")
        .expect("update command")
}

#[test]
fn update_flags_resolve_to_upstream_targets() {
    assert_eq!(update_request(&["update"]), UpdateRequest::SelfUpdate);
    assert_eq!(
        update_request(&["update", "self"]),
        UpdateRequest::SelfUpdate
    );
    assert_eq!(update_request(&["update", "pi"]), UpdateRequest::SelfUpdate);
    assert_eq!(
        update_request(&["update", "--models"]),
        UpdateRequest::Models
    );
    assert_eq!(
        update_request(&["update", "--extensions"]),
        UpdateRequest::Extensions
    );
    assert_eq!(update_request(&["update", "--all"]), UpdateRequest::All);
    assert_eq!(
        update_request(&["update", "npm:foo"]),
        UpdateRequest::Package("npm:foo".to_string())
    );
    assert_eq!(
        update_request(&["update", "--extension", "npm:foo"]),
        UpdateRequest::Extension("npm:foo".to_string())
    );
    assert_eq!(
        update_request(&["update", "--self"]),
        UpdateRequest::SelfUpdate
    );
    assert_eq!(
        update_request(&["update", "--self", "--extensions"]),
        UpdateRequest::All
    );
    // `self`/`pi` with `--extensions` widens to all (upstream updateTarget).
    assert_eq!(
        update_request(&["update", "self", "--extensions"]),
        UpdateRequest::All
    );
    assert_eq!(
        update_request(&["update", "pi", "--extensions"]),
        UpdateRequest::All
    );
}

#[test]
fn update_force_flag_is_carried_into_the_plan() {
    let plan = update_plan(&["update", "--all", "--force"]);
    assert_eq!(plan.request, UpdateRequest::All);
    assert!(plan.force);
    let plan = update_plan(&["update", "--force"]);
    assert_eq!(plan.request, UpdateRequest::SelfUpdate);
    assert!(plan.force);
    assert!(
        plan.show_extensions_skipped_note,
        "bare update keeps the upstream skipped-extensions note"
    );
}

#[test]
fn update_conflicting_targets_fail_upstream_style() {
    // Upstream `parsePackageCommand` collects conflictingOptions and exits 1;
    // every pair below is rejected by `resolve_update_plan`.
    for argv in [
        vec!["update", "--all", "npm:foo"],
        vec!["update", "--all", "--models"],
        vec!["update", "--all", "--extensions"],
        vec!["update", "--all", "--self"],
        vec!["update", "--all", "--extension", "npm:foo"],
        vec!["update", "--models", "--extensions"],
        vec!["update", "--models", "npm:foo"],
        vec!["update", "--models", "--self"],
        vec!["update", "npm:foo", "--models"],
        vec!["update", "npm:foo", "--all"],
        vec!["update", "npm:foo", "--extensions"],
        vec!["update", "npm:foo", "--self"],
        vec!["update", "--extension", "npm:foo", "npm:bar"],
        vec!["update", "--extension", "npm:foo", "--extensions"],
        vec!["update", "--extension", "npm:foo", "--self"],
    ] {
        let cli = parse_preprocessed(&argv);
        assert!(
            cli.resolve_update_plan().is_err(),
            "upstream rejects conflicting update targets: {argv:?}"
        );
        assert!(
            cli.update_request().is_none(),
            "a conflicting plan has no update request: {argv:?}"
        );
    }
}

#[test]
fn update_request_is_none_without_the_update_subcommand() {
    let cli = Cli::try_parse_from(["pir"]).expect("bare argv should parse");
    assert!(cli.update_request().is_none());
}

// ---------------------------------------------------------------------------
// 5. `pir mcp list` is its own report, separate from `pir mcp check`
// ---------------------------------------------------------------------------

#[test]
fn mcp_list_is_a_distinct_upstream_shaped_command() {
    fn mcp_command(argv: &[&str]) -> Option<pir::coding_agent::cli::McpCommand> {
        let mut full = vec!["pir"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full)
            .expect("argv should parse")
            .command
        {
            Some(pir::coding_agent::cli::Command::Mcp { command }) => command,
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // Upstream `pi mcp list [--json]`: a dedicated listing command.
    assert!(matches!(
        mcp_command(&["mcp", "list"]),
        Some(pir::coding_agent::cli::McpCommand::List { json: false })
    ));
    assert!(matches!(
        mcp_command(&["mcp", "list", "--json"]),
        Some(pir::coding_agent::cli::McpCommand::List { json: true })
    ));
    // `check <server>` keeps parsing exactly as before.
    match mcp_command(&["mcp", "check", "docs"]) {
        Some(pir::coding_agent::cli::McpCommand::Check { server, json }) => {
            assert_eq!(server.as_deref(), Some("docs"));
            assert!(!json);
        }
        other => panic!("unexpected mcp command: {other:?}"),
    }
    // No subcommand keeps the historical default (the host supplies Check).
    assert!(mcp_command(&["mcp"]).is_none());
}

// ---------------------------------------------------------------------------
// 6. Lane I: `mcp list`, `auth check`, honest self-update, package trust gate,
//    git dependency installation (audit §2 #5/#7/#10/#13/#14/#21)
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-lane-i-{tag}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Spawn the real binary with an isolated agent dir, cwd, and a clean auth
/// environment (ambient provider keys are removed; tests that need one set it).
fn run_pir(cwd: &Path, agent_dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(cwd)
        .env("PIR_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env_remove("PIR_TRUST_PROJECT")
        .env_remove("PI_OFFLINE");
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    command.args(args);
    command.output().expect("pir binary should start")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn mcp_list_fails_nonzero_and_reports_scope_exposure_state() {
    let agent = temp_dir("mcp-list-fail");
    std::fs::write(
        pir::config::mcp::global_mcp_path(&agent),
        r#"{"mcpServers":{"broken":{"command":"pir-lane-i-no-such-command"}}}"#,
    )
    .expect("write mcp.json");
    let output = run_pir(&agent, &agent, &["mcp", "list", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed server must exit 1: {}",
        stderr(&output)
    );
    let report: Value = serde_json::from_str(&stdout(&output)).expect("json report");
    let server = &report["servers"][0];
    assert_eq!(server["name"], "broken");
    assert_eq!(server["scope"], "global");
    assert_eq!(server["exposure"], "codemode");
    assert_eq!(server["enabled"], true);
    assert_eq!(server["state"], "failed");
    assert!(server["error"].is_string(), "error text: {server}");
    assert!(report["errors"].as_array().is_some_and(Vec::is_empty));
}

#[test]
fn mcp_list_disabled_server_succeeds_and_empty_list_is_informative() {
    let agent = temp_dir("mcp-list-disabled");
    std::fs::write(
        pir::config::mcp::global_mcp_path(&agent),
        r#"{"mcpServers":{"off":{"command":"true","enabled":false}}}"#,
    )
    .expect("write mcp.json");
    let output = run_pir(&agent, &agent, &["mcp", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("json report");
    assert_eq!(report["servers"][0]["state"], "disabled");
    assert!(report["servers"][0].get("error").is_none());

    let empty_agent = temp_dir("mcp-list-empty");
    let output = run_pir(&empty_agent, &empty_agent, &["mcp", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("No MCP servers configured"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn auth_check_matches_upstream_fields_and_exit_codes() {
    let agent = temp_dir("auth-check");

    // Unknown provider: not_ready/provider_not_found, exit 1.
    let output = run_pir(
        &agent,
        &agent,
        &[
            "auth",
            "check",
            "--provider",
            "pir-lane-i-unknown",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("json result");
    assert_eq!(report["status"], "not_ready");
    assert_eq!(report["provider"], "pir-lane-i-unknown");
    assert_eq!(report["reason"], "provider_not_found");

    // Neither flag: upstream AuthCommandError, exit 2.
    let output = run_pir(&agent, &agent, &["auth", "check", "--json"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("Auth checks require --provider"),
        "{}",
        stderr(&output)
    );

    // Unresolvable model: invalid/invalid_state, exit 2.
    let output = run_pir(
        &agent,
        &agent,
        &[
            "auth",
            "check",
            "--model",
            "pir-lane-i-no-such-model",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("json result");
    assert_eq!(report["status"], "invalid");
    assert_eq!(report["reason"], "invalid_state");

    // Configured env key: ready/authType api_key, exit 0; --credentials prints
    // the credential alone (upstream `credential ?? status`).
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(&agent)
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .env_remove("PIR_TRUST_PROJECT")
        .env("ANTHROPIC_API_KEY", "lane-i-test-key")
        .args(["auth", "check", "--provider", "anthropic", "--json"]);
    for key in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_OAUTH_TOKEN",
        "OPENAI_API_KEY",
    ] {
        command.env_remove(key);
    }
    let output = command.output().expect("pir binary should start");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("json result");
    assert_eq!(report["status"], "ready");
    assert_eq!(report["provider"], "anthropic");
    assert_eq!(report["authType"], "api_key");
    assert!(report.get("credentials").is_none());

    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(&agent)
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .env_remove("PIR_TRUST_PROJECT")
        .env("ANTHROPIC_API_KEY", "lane-i-test-key")
        .args(["auth", "check", "--provider", "anthropic", "--credentials"]);
    for key in ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_OAUTH_TOKEN"] {
        command.env_remove(key);
    }
    let output = command.output().expect("pir binary should start");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "lane-i-test-key");
}

#[test]
fn update_self_is_an_honest_nonzero_failure() {
    let agent = temp_dir("update-self");
    for args in [
        vec!["update"],
        vec!["update", "--self"],
        vec!["update", "--force"],
        vec!["update", "--all", "--force"],
    ] {
        let output = run_pir(&agent, &agent, &args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?} must not report success: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("cannot self-update"),
            "{args:?} stderr: {}",
            stderr(&output)
        );
    }
    // Bare `update` keeps upstream's skipped-extensions note.
    let output = run_pir(&agent, &agent, &["update"]);
    assert!(
        stdout(&output).contains("Extensions are skipped"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn update_extension_and_conflicts_follow_upstream_rules() {
    let agent = temp_dir("update-conflicts");
    for args in [
        vec!["update", "--all", "--extension", "npm:foo"],
        vec!["update", "--extension", "npm:foo", "--extensions"],
        vec!["update", "npm:foo", "--extensions"],
        vec!["update", "npm:foo", "--self"],
        vec!["update", "--models", "npm:foo"],
    ] {
        let output = run_pir(&agent, &agent, &args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?} must be rejected: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("cannot be combined"),
            "{args:?} stderr: {}",
            stderr(&output)
        );
    }
    // `--extension <source>` with no matching package reaches the update path
    // (and fails there), proving it parsed as an update target rather than as
    // the global extension path flag.
    let output = run_pir(&agent, &agent, &["update", "--extension", "npm:foo"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("No matching package found for npm:foo"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn local_package_config_is_gated_by_project_trust() {
    let project = temp_dir("trust-project");
    let agent = temp_dir("trust-agent");
    let package = temp_dir("trust-package");
    std::fs::create_dir_all(project.join(".pi")).expect("project .pi");
    let settings_path = project.join(".pi").join("settings.json");
    std::fs::write(&settings_path, "{}").expect("seed settings");

    // Untrusted project: `install --local` is refused before writing, and the
    // project settings are not modified.
    let package_arg = package.display().to_string();
    let output = run_pir(&project, &agent, &["install", &package_arg, "--local"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("Project is not trusted. Use --approve to modify local package config."),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(&settings_path).expect("settings readable"),
        "{}"
    );

    // `--approve` makes the write work.
    let output = run_pir(
        &project,
        &agent,
        &["install", &package_arg, "--local", "--approve"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("settings"))
            .expect("settings json");
    let packages = settings["packages"].as_array().expect("packages array");
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].as_str(), Some(package_arg.as_str()));

    // `config --local` uses the resource-config wording and the same gate.
    let output = run_pir(&project, &agent, &["config", "--local"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("Project is not trusted. Use --approve to modify local resource config."),
        "{}",
        stderr(&output)
    );
    let output = run_pir(&project, &agent, &["config", "--local", "--approve"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("Resource configuration"));
}

#[test]
fn list_ignores_untrusted_project_packages_unless_approved() {
    let project = temp_dir("list-trust-project");
    let agent = temp_dir("list-trust-agent");
    std::fs::create_dir_all(project.join(".pi")).expect("project .pi");
    std::fs::write(
        project.join(".pi").join("settings.json"),
        r#"{"packages":["npm:lane-i-secret-package"]}"#,
    )
    .expect("seed project settings");

    let output = run_pir(&project, &agent, &["list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !stdout(&output).contains("lane-i-secret-package"),
        "untrusted project settings must not be read: {}",
        stdout(&output)
    );

    let output = run_pir(&project, &agent, &["list", "--approve"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("lane-i-secret-package"),
        "--approve must read the project settings: {}",
        stdout(&output)
    );
}

#[test]
fn git_dependency_install_args_match_upstream_package_managers() {
    use pir::coding_agent::package_manager::git_dependency_install_args;
    assert_eq!(
        git_dependency_install_args("npm"),
        vec!["install", "--omit=dev", "--legacy-peer-deps"]
    );
    assert_eq!(
        git_dependency_install_args("pnpm"),
        vec![
            "install",
            "--prod",
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ]
    );
    assert_eq!(
        git_dependency_install_args("bun"),
        vec!["install", "--omit=dev", "--omit=peer"]
    );
    assert_eq!(git_dependency_install_args("unknown"), vec!["install"]);

    // The missing-dependency probe reads `dependencies` only and treats an
    // absent node_modules entry as missing (upstream hasMissingGitDependencies).
    let package = temp_dir("git-deps");
    std::fs::write(
        package.join("package.json"),
        r#"{"dependencies":{"@scope/dep":"1.0.0"}}"#,
    )
    .expect("package.json");
    assert!(pir::coding_agent::package_manager::git_package_has_missing_dependencies(&package));
    std::fs::create_dir_all(package.join("node_modules").join("@scope").join("dep"))
        .expect("node_modules");
    assert!(!pir::coding_agent::package_manager::git_package_has_missing_dependencies(&package));
}

#[test]
fn git_error_text_redacts_embedded_credentials() {
    use pir::coding_agent::package_manager::redact_url_credentials;
    assert_eq!(
        redact_url_credentials(
            "fatal: repository 'https://user:token@example.com/org/repo' not found"
        ),
        "fatal: repository 'https://***@example.com/org/repo' not found"
    );
    // A bare token in the userinfo slot (no colon) is also a credential.
    assert_eq!(
        redact_url_credentials("clone https://ghp_secret@example.com/org/repo failed"),
        "clone https://***@example.com/org/repo failed"
    );
    // URLs without userinfo and non-URL text stay untouched.
    assert_eq!(
        redact_url_credentials("https://example.com/org/repo"),
        "https://example.com/org/repo"
    );
    assert_eq!(redact_url_credentials("plain failure"), "plain failure");
}

#[test]
fn git_dependency_install_warns_without_npm_and_runs_the_configured_command() {
    use pir::coding_agent::package_manager::install_git_dependencies;
    use pir::config::settings::SettingsManager;

    let package = temp_dir("git-install-deps");
    std::fs::write(
        package.join("package.json"),
        r#"{"dependencies":{"left-pad":"1.0.0"}}"#,
    )
    .expect("package.json");
    let global = temp_dir("git-install-deps-agent");
    let project = package.join(".pi");

    // A missing npm command degrades to a warning and still succeeds, so the
    // git package install completes (task §2 #21 fallback).
    let mut settings = SettingsManager::load(&global, &project, &package, false);
    // A path inside the temp dir cannot be shadowed by a sibling test's cwd or
    // PATH state: bare names are resolved by execvp and can race with tests
    // that temporarily create same-named entries.
    let missing_npm = package.join("pir-lane-i-missing-npm");
    settings.settings.npm_command = Some(vec![missing_npm.to_string_lossy().into_owned()]);
    install_git_dependencies(&package, &settings)
        .expect("a missing npm binary must not fail the git install");

    // The configured command runs in the cloned package directory with the
    // upstream production-dependency argv.
    #[cfg(windows)]
    {
        let log = package.join("npm-args.txt");
        let script = package.join("fake-npm.bat");
        std::fs::write(
            &script,
            "@echo off\r\necho %* > \"%PIR_LANE_I_NPM_LOG%\"\r\necho %CD% >> \"%PIR_LANE_I_NPM_LOG%\"\r\n",
        )
        .expect("fake npm script");
        std::env::set_var("PIR_LANE_I_NPM_LOG", &log);
        let mut settings = SettingsManager::load(&global, &project, &package, false);
        settings.settings.npm_command = Some(vec![
            "cmd".to_string(),
            "/c".to_string(),
            script.display().to_string(),
        ]);
        install_git_dependencies(&package, &settings).expect("configured npm command runs");
        std::env::remove_var("PIR_LANE_I_NPM_LOG");
        let recorded = std::fs::read_to_string(&log).expect("npm invocation log");
        assert!(
            recorded.contains("install"),
            "upstream git dependency argv: {recorded}"
        );
        assert!(
            recorded.contains(&package.display().to_string()),
            "npm must run inside the cloned package: {recorded}"
        );
    }
}
