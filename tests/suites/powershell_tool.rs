//! PowerShell tool (item 3 of the port stream).
//!
//! Mirrors `packages/coding-agent/src/core/tools/powershell.ts` +
//! `utils/shell.ts` `getPowerShellConfig`/`POWERSHELL_ARGS`: the tool name is
//! `powershell`, it is registered but NOT active by default (upstream
//! `DEFAULT_TOOL_NAMES = ["read", "bash", "edit", "write"]`), and it is
//! selectable through the `--tools` allowlist.

use pir::agent::tool_execution::ToolRegistry;
use pir::coding_agent::tools::register_core_tools;
use pir::coding_agent::{resolve_tool_selection, ToolSelection};
use pir::server::{Server, ServerSecurityConfig};

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

fn tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Registration and default activity (upstream `createTools` + DEFAULT_TOOL_NAMES)
// ---------------------------------------------------------------------------

#[test]
fn powershell_is_registered_alongside_the_other_core_tools() {
    let registry = registry();
    let names = tool_names(&registry);
    for expected in [
        "bash",
        "edit",
        "find",
        "grep",
        "ls",
        "powershell",
        "read",
        "write",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "registry is missing {expected}: {names:?}"
        );
    }
    let tool = registry.get("powershell").expect("powershell registered");
    assert_eq!(tool.name(), "powershell");
}

#[test]
fn powershell_is_not_active_by_default() {
    let registry = registry();
    let active = registry.active_names(None);
    assert!(
        !active.iter().any(|name| name == "powershell"),
        "powershell must stay out of the default active set: {active:?}"
    );
    // The upstream defaults remain active.
    for expected in ["bash", "edit", "read", "write"] {
        assert!(
            active.iter().any(|name| name == expected),
            "{expected} should be active by default: {active:?}"
        );
    }
}

#[test]
fn powershell_declares_the_upstream_schema() {
    let registry = registry();
    let tool = registry.get("powershell").expect("powershell registered");
    let schema = tool.parameters().expect("parameters schema");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["command"]["type"], "string");
    // Upstream shares the bash schema: the timeout is a NUMBER (fractional
    // seconds allowed), not an integer.
    assert_eq!(schema["properties"]["timeout"]["type"], "number");
    assert_eq!(
        schema["required"],
        serde_json::json!(["command"]),
        "the command argument is required"
    );
}

// ---------------------------------------------------------------------------
// Allowlist selection (upstream `--tools powershell`)
// ---------------------------------------------------------------------------

#[test]
fn tools_allowlist_selects_powershell() {
    let registry = registry();
    let available = tool_names(&registry);
    let selection = resolve_tool_selection(
        None,
        &["powershell".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("selection resolves")
    .expect("a selection was requested");
    assert_eq!(selection.enabled, vec!["powershell".to_string()]);

    // The selection activates the otherwise-default-off tool.
    let active = registry.active_names(Some(&selection.enabled));
    assert_eq!(active, vec!["powershell".to_string()]);
}

#[test]
fn tools_allowlist_can_combine_powershell_with_bash() {
    let registry = registry();
    let available = tool_names(&registry);
    let selection = resolve_tool_selection(
        None,
        &["bash".to_string(), "powershell".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("selection resolves")
    .expect("a selection was requested");
    let active = registry.active_names(Some(&selection.enabled));
    assert_eq!(active, vec!["bash".to_string(), "powershell".to_string()]);
}

#[test]
fn tools_modifier_can_add_powershell_to_the_defaults() {
    let registry = registry();
    let available = tool_names(&registry);
    let selection = resolve_tool_selection(
        None,
        &["+powershell".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("selection resolves")
    .expect("a selection was requested");
    let active = registry.active_names(Some(&selection.enabled));
    assert!(
        active.iter().any(|name| name == "powershell"),
        "+powershell must add the tool: {active:?}"
    );
    assert!(active.iter().any(|name| name == "bash"), "{active:?}");
}

#[test]
fn exclude_tools_can_drop_powershell() {
    let registry = registry();
    let available = tool_names(&registry);
    let selection = resolve_tool_selection(
        None,
        &[],
        &["powershell".to_string()],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("selection resolves")
    .expect("a selection was requested");
    assert!(
        !selection.enabled.iter().any(|name| name == "powershell"),
        "--exclude-tools powershell must drop it: {:?}",
        selection.enabled
    );
}

#[test]
fn tool_selection_type_is_public_for_hosts() {
    // The resolved selection is the host's activation override; keep the type
    // name stable for the print/TUI hosts.
    let selection = ToolSelection {
        enabled: vec!["powershell".to_string()],
        ..Default::default()
    };
    assert_eq!(selection.enabled.len(), 1);
}

// ---------------------------------------------------------------------------
// Server host security policy (`allow_remote_bash`)
// ---------------------------------------------------------------------------

/// Regression (D11/H1): the server's fail-closed remote-execution policy must
/// remove BOTH shell-execution tools. Before this fix only `bash` was removed,
/// so `powershell` stayed registered (default-inactive, undeclared) and a
/// model-emitted call to it still executed on the host. The positive path
/// (`allow_remote_bash: true`) must keep both.
#[tokio::test]
async fn server_session_excludes_both_shell_tools_unless_remote_bash_allowed() {
    let locked_down = Server::bind("127.0.0.1:0")
        .await
        .expect("bind default test server");
    locked_down
        .state()
        .create_session("shell-excluded".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let names = locked_down
        .state()
        .session_tool_names("shell-excluded")
        .await
        .expect("session exists");
    assert!(!names.is_empty(), "the core tools must be registered");
    for shell in ["bash", "powershell"] {
        assert!(
            !names.iter().any(|name| name == shell),
            "{shell} must be excluded when allow_remote_bash is off, got {names:?}"
        );
    }

    let permissive = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            allow_remote_bash: true,
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind permissive test server");
    permissive
        .state()
        .create_session("shell-allowed".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let names = permissive
        .state()
        .session_tool_names("shell-allowed")
        .await
        .expect("session exists");
    for shell in ["bash", "powershell"] {
        assert!(
            names.iter().any(|name| name == shell),
            "allow_remote_bash must keep {shell}, got {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Real execution on Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_execution {
    use super::*;
    use pir::agent::tool_batch::ToolResult;
    use pir::agent::tool_execution::{AgentTool, ToolContext};
    use std::sync::Arc;

    fn powershell_tool() -> Arc<dyn AgentTool> {
        registry().get("powershell").expect("powershell registered")
    }

    fn context(cwd: &std::path::Path) -> ToolContext {
        ToolContext::new(cwd.to_path_buf())
    }

    fn text_of(result: &ToolResult) -> String {
        result.content.clone()
    }

    #[tokio::test]
    async fn powershell_executes_a_real_command() {
        let tool = powershell_tool();
        let cwd = std::env::temp_dir();
        let result = tool
            .run(
                &serde_json::json!({ "command": "Write-Output 'pir-powershell-ok'" }),
                &context(&cwd),
            )
            .await;
        assert!(!result.is_error, "run failed: {}", text_of(&result));
        assert!(
            text_of(&result).contains("pir-powershell-ok"),
            "unexpected output: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn powershell_reports_a_non_zero_exit_code() {
        let tool = powershell_tool();
        let cwd = std::env::temp_dir();
        let result = tool
            .run(&serde_json::json!({ "command": "exit 3" }), &context(&cwd))
            .await;
        assert!(result.is_error, "a non-zero exit must be an error result");
        assert!(
            text_of(&result).contains("Command exited with code 3"),
            "unexpected output: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn powershell_rejects_an_empty_command() {
        let tool = powershell_tool();
        let cwd = std::env::temp_dir();
        let result = tool
            .run(&serde_json::json!({ "command": "   " }), &context(&cwd))
            .await;
        assert!(result.is_error);
        assert!(
            text_of(&result).contains("command is required"),
            "unexpected output: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn powershell_rejects_an_invalid_timeout() {
        let tool = powershell_tool();
        let cwd = std::env::temp_dir();
        let result = tool
            .run(
                &serde_json::json!({ "command": "Write-Output hi", "timeout": 0 }),
                &context(&cwd),
            )
            .await;
        assert!(result.is_error);
        assert!(
            text_of(&result).contains("Invalid timeout"),
            "unexpected output: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn powershell_rejects_a_missing_working_directory() {
        let tool = powershell_tool();
        let cwd = std::env::temp_dir().join("pir-powershell-missing-cwd-xyz");
        let result = tool
            .run(
                &serde_json::json!({ "command": "Write-Output hi" }),
                &context(&cwd),
            )
            .await;
        assert!(result.is_error);
        assert!(
            text_of(&result).contains("does not exist"),
            "unexpected output: {:?}",
            text_of(&result)
        );
    }
}
