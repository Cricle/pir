//! R lane: project-trust ordering for metadata commands (§2 #19 / §7 #21).
//!
//! Upstream resolves project trust before a run starts, but sets
//! `trustPromptMode = "print"` for `--help`/`--list-models`
//! (main.ts:726,773) and handles `--export` before the runtime
//! (main.ts:640-652). The selector therefore never blocks a metadata command
//! on a TTY: trust resolves through overrides/the store/`defaultProjectTrust`
//! first, then the command runs. These tests lock that policy and the
//! resolution-before-command ordering through the shared
//! `resolve_run_trust` seam.

use std::path::PathBuf;

use clap::Parser;
use pir::coding_agent::cli::Cli;
use pir::config::settings::DefaultProjectTrust;
use pir::config::trust::{
    get_project_trust_options, get_project_trust_options_including_session_only, resolve_run_trust,
    ProjectTrustStore,
};

fn parse(argv: &[&str]) -> Cli {
    let mut full = vec!["pir"];
    full.extend_from_slice(argv);
    Cli::try_parse_from(full).expect("argv should parse")
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-run-trust-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A project with trust-requiring resources (`.pi/settings.json`).
fn trust_requiring_project(label: &str) -> PathBuf {
    let project = temp_dir(label);
    std::fs::create_dir_all(project.join(".pi")).expect("create .pi");
    std::fs::write(project.join(".pi").join("settings.json"), "{}").expect("write settings");
    project
}

#[test]
fn metadata_commands_never_offer_the_trust_selector_even_on_a_tty() {
    for argv in [
        vec!["--list-models"],
        vec!["--list-models", "gpt"],
        vec!["--export", "session.jsonl"],
    ] {
        let cli = parse(&argv);
        assert!(cli.is_metadata_command(), "{argv:?} is a metadata command");
        assert!(
            !cli.trust_prompt_has_ui(true),
            "{argv:?} must resolve trust without a TTY prompt"
        );
        assert!(!cli.trust_prompt_has_ui(false));
    }
    // A normal interactive run keeps the prompt.
    for argv in [vec![], vec!["interactive"]] {
        let cli = parse(&argv);
        assert!(!cli.is_metadata_command(), "{argv:?} is not metadata");
        assert!(cli.trust_prompt_has_ui(true), "{argv:?} keeps the prompt");
        assert!(
            !cli.trust_prompt_has_ui(false),
            "{argv:?} never prompts without a TTY"
        );
    }
    // Print is not an interactive run either, though it is not metadata.
    let print = parse(&["print", "hello"]);
    assert!(!print.is_metadata_command());
    assert!(!print.trust_prompt_has_ui(false));
}

#[test]
fn trust_resolves_before_the_command_and_metadata_never_prompts() {
    let agent = temp_dir("agent");
    let project = trust_requiring_project("project");
    let store = ProjectTrustStore::new(&agent);
    // The stored "do not trust" decision is the one consulted; the command
    // then runs against an untrusted project scope.
    store.set(&project, Some(false)).expect("store decision");

    // A metadata command on a TTY: no prompt, the stored decision resolves.
    let cli = parse(&["--list-models"]);
    let mut prompted = false;
    let resolution = resolve_run_trust(
        &project,
        &store,
        None,
        DefaultProjectTrust::Ask,
        cli.trust_prompt_has_ui(true),
        |_options| {
            prompted = true;
            Some(0)
        },
    );
    assert!(!prompted, "the selector must not run for --list-models");
    assert!(!resolution.prompted);
    assert!(!resolution.trusted, "the stored decision is honored");
    assert_eq!(
        store.get(&project).map(|entry| entry.decision),
        Some(false),
        "the decision was read before the command ran and was not rewritten"
    );

    // The same project on a plain interactive run DOES prompt (trust first,
    // then the command), and the choice is persisted. Clear the stored
    // decision first: a saved decision resolves without asking.
    store.set(&project, None).expect("clear decision");
    let cli = parse(&[]);
    let mut prompted = false;
    let resolution = resolve_run_trust(
        &project,
        &store,
        None,
        DefaultProjectTrust::Ask,
        cli.trust_prompt_has_ui(true),
        |options| {
            prompted = true;
            Some(
                options
                    .iter()
                    .position(|option| option.label == "Do not trust")
                    .expect("persistent 'Do not trust' row"),
            )
        },
    );
    assert!(prompted, "a plain interactive run keeps the trust prompt");
    assert!(resolution.prompted);
    assert!(!resolution.trusted);
    assert_eq!(store.get(&project).map(|entry| entry.decision), Some(false));

    // An explicit override resolves without any prompt.
    let resolution = resolve_run_trust(
        &project,
        &store,
        Some(true),
        DefaultProjectTrust::Never,
        true,
        |_options| panic!("--approve must not prompt"),
    );
    assert!(resolution.trusted);
    assert!(!resolution.prompted);
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&project);
}

/// §2 #12: the startup trust selector offers upstream's "this session only"
/// rows, and choosing one decides the current run without writing
/// `trust.json` (upstream `getProjectTrustOptions(..., {
/// includeSessionOnly: true })` + `saveProjectTrustPromptResult` skipping
/// empty `updates`).
#[test]
fn session_only_trust_options_do_not_persist_a_decision() {
    let agent = temp_dir("agent-session-only");
    let project = trust_requiring_project("project-session-only");
    let store = ProjectTrustStore::new(&agent);

    // Upstream option order: Trust, Trust parent, Trust (this session only),
    // Do not trust, Do not trust (this session only). The `/trust` selector
    // list stays persistent-only.
    let prompt_options = get_project_trust_options_including_session_only(&project);
    let labels: Vec<&str> = prompt_options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(labels[0], "Trust");
    assert!(labels[1].starts_with("Trust parent folder"));
    assert_eq!(labels[2], "Trust (this session only)");
    assert_eq!(labels[3], "Do not trust");
    assert_eq!(labels[4], "Do not trust (this session only)");
    assert!(prompt_options[2].session_only);
    assert!(prompt_options[4].session_only);
    assert!(prompt_options[2].updates.is_empty());
    assert!(prompt_options[4].updates.is_empty());
    assert_eq!(get_project_trust_options(&project).len(), 3);

    // "Trust (this session only)": trusted for this run, store untouched
    // (and never created).
    let resolution = resolve_run_trust(
        &project,
        &store,
        None,
        DefaultProjectTrust::Ask,
        true,
        |options| {
            Some(
                options
                    .iter()
                    .position(|option| option.label == "Trust (this session only)")
                    .expect("session-only trust row"),
            )
        },
    );
    assert!(resolution.prompted);
    assert!(
        resolution.trusted,
        "the one-time decision applies to this run"
    );
    assert!(
        store.get(&project).is_none(),
        "a session-only decision must not be stored"
    );
    assert!(
        !store.path().exists(),
        "a session-only decision must not even create trust.json"
    );

    // "Do not trust (this session only)": untrusted for this run, untouched
    // store.
    let resolution = resolve_run_trust(
        &project,
        &store,
        None,
        DefaultProjectTrust::Ask,
        true,
        |options| {
            Some(
                options
                    .iter()
                    .position(|option| option.label == "Do not trust (this session only)")
                    .expect("session-only distrust row"),
            )
        },
    );
    assert!(resolution.prompted);
    assert!(!resolution.trusted);
    assert!(store.get(&project).is_none());
    assert!(!store.path().exists());

    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn list_models_in_a_trust_requiring_project_prints_no_prompt() {
    // Process-level lock: `--list-models` and the trust store must see a
    // hermetic agent dir/home, and this test mutates only child-process env.
    let agent = temp_dir("agent-process");
    let project = trust_requiring_project("project-process");
    // `defaultProjectTrust: ask` would prompt on a TTY; the non-TTY run must
    // resolve to "not trusted" without printing the selector.
    std::fs::write(
        agent.join("settings.json"),
        r#"{"defaultProjectTrust":"ask"}"#,
    )
    .expect("write agent settings");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("--list-models")
        .current_dir(&project)
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .env_remove("PIR_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_DIR")
        .output()
        .expect("pir binary should start");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("Trust project folder?") && !stderr.contains("Trust project folder?"),
        "a metadata command must never show the trust selector: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        output.status.success(),
        "--list-models must still run: stdout={stdout:?} stderr={stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&project);
}

/// `--list-models` is a metadata command: it must print the model table
/// without resolving the extension plan, which would install every configured
/// package before the command runs. The settings here name a package plus a
/// deliberately missing npm command, so any attempt to resolve packages is
/// both observable (an "Extension package … failed to install" warning) and
/// offline-safe (the spawn fails immediately instead of touching the network).
#[test]
fn list_models_does_not_resolve_or_install_configured_packages() {
    let agent = temp_dir("no-install-agent");
    let project = temp_dir("no-install-project");
    std::fs::write(
        agent.join("settings.json"),
        r#"{
            "packages": ["npm:pir-lane-e-package-that-does-not-exist"],
            "npmCommand": ["pir-lane-e-npm-that-does-not-exist"]
        }"#,
    )
    .expect("write agent settings");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("--list-models")
        .current_dir(&project)
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", &agent)
        .env("USERPROFILE", &agent)
        .env_remove("PIR_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("PIR_OFFLINE")
        .env_remove("PIR_TRUST_PROJECT")
        .output()
        .expect("pir binary should start");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "--list-models must succeed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stderr.contains("failed to install"),
        "--list-models must not attempt a package install: stderr={stderr:?}"
    );
    assert!(
        !agent.join("npm").exists(),
        "the package manager must not create an install root for a metadata command"
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&project);
}
