//! `--only-builtin` semantics.
//!
//! The flag keeps the Rust builtin plugins (the same default-enabled set the
//! normal plan carries) while loading NO on-disk extension files and NO
//! installed packages. It is the middle ground between the default plan (JS
//! packages load) and `--no-extensions` (no builtins either):
//!
//! - the plan is `-e builtin:<name>` entries (deduped, CLI order) followed by
//!   the default-enabled builtins;
//! - on-disk `-e` paths are ignored and reported in one diagnostic;
//! - the handle built from that plan is plan-only: no QuickJS actor, no
//!   prelude, no SWC transpile;
//! - every builtin's native tools/commands are still registered;
//! - `--no-extensions` wins when both flags are given.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;
use pir::coding_agent::cli::Cli;

fn parse_cli(argv: &[&str]) -> Cli {
    let mut full = vec!["pir"];
    full.extend_from_slice(argv);
    Cli::try_parse_from(full).expect("argv should parse")
}

fn builtin_path(name: &str) -> PathBuf {
    PathBuf::from(format!("builtin:{name}"))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-only-builtin-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn extension_config(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: None,
    }
}

// ---------------------------------------------------------------------------
// Flag parsing + precedence
// ---------------------------------------------------------------------------

#[test]
fn flag_parses_and_no_extensions_wins() {
    assert!(parse_cli(&["--only-builtin"]).only_builtin);
    assert!(!parse_cli(&[]).only_builtin);
    // Global flag: accepted after a subcommand too.
    assert!(parse_cli(&["print", "hi", "--only-builtin"]).only_builtin);
    // `--no-extensions` wins when both are set.
    assert_eq!(
        pir::plugins::extension_plan_mode(true, true),
        pir::plugins::ExtensionPlanMode::NoExtensions
    );
    assert_eq!(
        pir::plugins::extension_plan_mode(true, false),
        pir::plugins::ExtensionPlanMode::NoExtensions
    );
    assert_eq!(
        pir::plugins::extension_plan_mode(false, true),
        pir::plugins::ExtensionPlanMode::OnlyBuiltin
    );
    assert_eq!(
        pir::plugins::extension_plan_mode(false, false),
        pir::plugins::ExtensionPlanMode::Default
    );
    // `--no-extensions` degrades to its historical behavior: only the CLI
    // paths (an explicit file still loads), no builtins injected.
    let plan = pir::plugins::order_extension_paths(
        &[PathBuf::from("cli.ts")],
        &[PathBuf::from("defaults")],
        true,
    );
    assert_eq!(plan, vec![PathBuf::from("cli.ts")]);
}

#[test]
fn help_lists_the_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("--help")
        .output()
        .expect("pir --help should run");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--only-builtin"),
        "help omitted --only-builtin: {text}"
    );
    assert!(
        text.contains("--no-extensions"),
        "help omitted --no-extensions: {text}"
    );
}

// ---------------------------------------------------------------------------
// Plan shape
// ---------------------------------------------------------------------------

#[test]
fn plan_keeps_only_builtins_and_reports_on_disk_paths() {
    let names = pir::plugins::builtin_names();
    if names.is_empty() {
        // A minimal build compiles no builtin factories; nothing to exercise.
        return;
    }
    let explicit = builtin_path("todo");
    let file = PathBuf::from("C:/tmp/pi-extension.js");
    let (plan, ignored) = pir::plugins::only_builtin_paths(&[
        file.clone(),
        explicit.clone(),
        PathBuf::from("builtin:todo"),
    ]);

    assert_eq!(ignored, vec![file], "the on-disk path is ignored");
    assert!(
        plan.iter()
            .all(|path| path.to_string_lossy().starts_with("builtin:")),
        "the plan contains only builtin pseudo-paths: {plan:?}"
    );
    assert_eq!(
        plan.iter().filter(|path| **path == explicit).count(),
        1,
        "the explicit builtin entry is deduped into its CLI position"
    );
    assert_eq!(plan.first(), Some(&explicit), "CLI builtins keep order");
    // Every default-enabled builtin is present (phone stays default-off).
    for name in &names {
        if *name == "phone" {
            continue;
        }
        assert!(
            plan.contains(&builtin_path(name)),
            "default builtin `{name}` missing from {plan:?}"
        );
    }
}

#[test]
fn explicit_builtin_phone_is_kept_under_the_flag() {
    let (plan, ignored) = pir::plugins::only_builtin_paths(&[builtin_path("phone")]);
    assert!(ignored.is_empty());
    assert_eq!(plan.first(), Some(&builtin_path("phone")));
}

#[test]
fn sdk_filter_keeps_builtins_without_injecting_defaults() {
    let (plan, ignored) = pir::plugins::filter_builtin_paths(&[
        PathBuf::from("ext.ts"),
        builtin_path("todo"),
        builtin_path("todo"),
    ]);
    assert_eq!(ignored, vec![PathBuf::from("ext.ts")]);
    assert_eq!(plan, vec![builtin_path("todo")]);
}

// ---------------------------------------------------------------------------
// No JS runtime, native builtin surface still registered
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn plan_only_handle_registers_native_builtins() {
    let temp = TempDir::new("native");
    let names = pir::plugins::builtin_names();
    if names.is_empty() {
        return;
    }
    let (plan, ignored) = pir::plugins::only_builtin_paths(&[PathBuf::from("ignored.js")]);
    assert_eq!(ignored.len(), 1);
    assert!(!plan.is_empty());

    let baseline = pir::extensions::runtime::js_runtime_build_count();
    let handle = pir::agent_session::load_extension_runtime(&plan, extension_config(&temp.0))
        .await
        .expect("a builtin-only plan still yields a handle");
    assert_eq!(
        pir::extensions::runtime::js_runtime_build_count(),
        baseline,
        "--only-builtin must not construct the QuickJS runtime"
    );
    assert!(
        !handle.has_js_runtime(),
        "--only-builtin must never start the JS actor"
    );
    assert!(handle.is_ready(), "the plan-only handle is usable");
    assert_eq!(handle.path_plan(), plan, "the plan is preserved");

    let host = pir::plugins::PluginHost::build(Some(handle), Vec::new());
    let plugin_paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    for name in &names {
        // `phone` is default-off: only its explicit `-e builtin:phone` adds it.
        if *name == "phone" {
            continue;
        }
        assert!(
            plugin_paths.contains(&format!("builtin:{name}")),
            "builtin `{name}` missing from the host: {plugin_paths:?}"
        );
    }

    // Native tools/commands are available without any JS engine.
    let registry = host.registry();
    let tools: Vec<String> = registry
        .resolved_tools()
        .iter()
        .map(|entry| entry.0.name.clone())
        .collect();
    for (builtin, tool) in [
        ("subagent", "subagent"),
        ("todo", "todo"),
        ("tool-search", "tool_search"),
    ] {
        if names.contains(&builtin) {
            assert!(
                tools.iter().any(|name| name == tool),
                "native tool `{tool}` missing from {tools:?}"
            );
        }
    }
    if names.contains(&"todo") {
        let commands: Vec<String> = registry
            .resolved_commands()
            .iter()
            .map(|command| command.invocation_name.clone())
            .collect();
        assert!(
            commands.iter().any(|command| command == "todos"),
            "the todo builtin's /todos command is missing: {commands:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// End-to-end diagnostic
// ---------------------------------------------------------------------------

fn run_in_hermetic_dir(agent_dir: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pir"))
        .env("PIR_AGENT_DIR", agent_dir)
        .env("PIR_SESSION_DIR", agent_dir.join("sessions"))
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PIR_TRUST_PROJECT")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("pir binary should start")
}

#[test]
fn on_disk_extension_is_ignored_with_one_diagnostic() {
    let agent = TempDir::new("agent");
    let project = TempDir::new("project");
    let extension = agent.0.join("ignored-extension.js");
    std::fs::write(
        &extension,
        "export default function (pi) { pi.registerCommand(\"never\", { handler() {} }); }\n",
    )
    .expect("write extension fixture");

    // No model is configured in the hermetic agent dir, so the run fails at
    // model resolution — AFTER the plan was resolved and the diagnostic
    // emitted. That is exactly the part under test.
    let output = run_in_hermetic_dir(
        &agent.0,
        &project.0,
        &[
            "print",
            "hi",
            "--only-builtin",
            "-e",
            extension.to_str().expect("utf-8 path"),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--only-builtin ignores 1 on-disk extension path(s)"),
        "missing the ignored-path diagnostic: {stderr}"
    );
    assert!(
        stderr.contains(extension.to_string_lossy().as_ref()),
        "the diagnostic must name the ignored path: {stderr}"
    );
    assert!(!output.status.success(), "no model is configured");
}

#[test]
fn explicit_builtin_under_the_flag_produces_no_ignore_diagnostic() {
    let agent = TempDir::new("agent-builtin");
    let project = TempDir::new("project-builtin");
    let output = run_in_hermetic_dir(
        &agent.0,
        &project.0,
        &["print", "hi", "--only-builtin", "-e", "builtin:todo"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("--only-builtin ignores"),
        "an explicit builtin is not ignored: {stderr}"
    );
    assert!(
        stderr.contains("--only-builtin: loading only the Rust builtin extensions"),
        "the flag announces the skipped discovery once: {stderr}"
    );
}
