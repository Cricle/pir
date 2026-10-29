//! Cross-builtin strict-verification tests: all three builtins (subagent,
//! mcp, codemode) load together through the unified [`PluginHost`], and the
//! builtin-replacement omission emits the exact upstream warning text
//! (upstream `resource-loader.ts` `omitReplacedExtensions`).

use std::path::{Path, PathBuf};

use pir::extensions::ExtensionHandle;
use pir::plugins::PluginHost;
use serde_json::json;

// ---------------------------------------------------------------------------
// Harness (same pattern as tests/plugins_unified.rs)
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-cross-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config_at(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("fake-model".to_string()),
        session_id: Some("cross-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn load_runtime(dir: &TempDir, sources: &[(&str, &str)]) -> ExtensionHandle {
    let runtime = ExtensionHandle::start(test_config_at(&dir.0)).expect("runtime");
    let mut paths = Vec::new();
    for (name, source) in sources {
        let path = dir.0.join(name);
        std::fs::write(&path, source).expect("write fixture");
        paths.push(path);
    }
    let result = runtime.load(&paths).await.expect("load extensions");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    runtime
}

// ---------------------------------------------------------------------------
// All three builtins together
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn all_three_builtins_load_together_with_one_shared_registry() {
    let temp = TempDir::new("all-three");
    let source = r#"
        export default function (pi) {
            pi.registerTool({
                name: "js-tool",
                description: "js probe",
                parameters: { type: "object" },
                execute: async () => ({ content: [{ type: "text", text: "from-js" }] }),
            });
            pi.on("input", (event) => ({ action: "transform", text: event.text + "[js]" }));
        }
    "#;
    let runtime = load_runtime(&temp, &[("ext.ts", source)]).await;
    runtime.set_path_plan(vec![
        PathBuf::from("builtin:subagent"),
        PathBuf::from("builtin:mcp"),
        PathBuf::from("builtin:codemode"),
        temp.0.join("ext.ts"),
    ]);
    let host = PluginHost::build(Some(runtime), Vec::new());

    // Dispatch order follows the path plan; no collisions, no diagnostics.
    let paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "builtin:subagent".to_owned(),
            "builtin:mcp".to_owned(),
            "builtin:codemode".to_owned(),
            "<inline:js-extensions>".to_owned(),
        ],
        "dispatch order: {paths:?}"
    );
    assert!(
        host.diagnostics().is_empty(),
        "no load diagnostics: {:?}",
        host.diagnostics()
    );

    // One shared registry: the Rust builtin tools and the JS tool coexist
    // (the mcp builtin registers no tools before a server connects).
    let mut tool_names: Vec<String> = host
        .tool_defs()
        .iter()
        .map(|def| def.name.clone())
        .collect();
    tool_names.sort();
    assert!(
        tool_names.contains(&"subagent".to_owned()),
        "{tool_names:?}"
    );
    assert!(
        tool_names.contains(&"codemode".to_owned()),
        "{tool_names:?}"
    );
    assert!(tool_names.contains(&"js-tool".to_owned()), "{tool_names:?}");
    assert!(
        !tool_names.iter().any(|name| name.starts_with("mcp__")),
        "no MCP tools before discovery: {tool_names:?}"
    );

    // The mcp builtin's manager is wired to the host.
    assert!(host.mcp_manager().is_some());

    // Event dispatch walks the ordered list: the JS input handler merges its
    // transform and no builtin breaks the chain.
    let mut payload = json!({ "text": "hello" });
    let outcome = host.emit("input", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(payload["text"], json!("hello[js]"));

    // Every builtin resolves through `builtin:<name>` addressing.
    for name in ["subagent", "mcp", "codemode", "tool-search"] {
        let plugin = pir::plugins::instantiate_builtin(name).expect(name);
        assert_eq!(plugin.info().path, format!("builtin:{name}"));
        assert!(plugin.info().replaceable, "{name} is replaceable");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_builtin_registry_rebuilds_do_not_drop_the_other_builtins() {
    let temp = TempDir::new("rebuild");
    let runtime = load_runtime(
        &temp,
        &[("ext.ts", "export default function (pi) { void pi; };")],
    )
    .await;
    runtime.set_path_plan(vec![
        PathBuf::from("builtin:subagent"),
        PathBuf::from("builtin:mcp"),
        PathBuf::from("builtin:codemode"),
        temp.0.join("ext.ts"),
    ]);
    let host = PluginHost::build(Some(runtime), Vec::new());

    // A registry rebuild (post-event refresh) keeps every builtin's
    // registrations.
    host.rebuild_registry().await;
    let mut tool_names: Vec<String> = host
        .tool_defs()
        .iter()
        .map(|def| def.name.clone())
        .collect();
    tool_names.sort();
    assert!(
        tool_names.contains(&"subagent".to_owned()),
        "{tool_names:?}"
    );
    assert!(
        tool_names.contains(&"codemode".to_owned()),
        "{tool_names:?}"
    );

    // A session_start dispatch triggers the mcp builtin's lazy discovery
    // (no servers configured: a no-op) without erroring the chain.
    let mut payload = json!({ "type": "session_start" });
    let outcome = host
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let mut tool_names: Vec<String> = host
        .tool_defs()
        .iter()
        .map(|def| def.name.clone())
        .collect();
    tool_names.sort();
    assert!(
        tool_names.contains(&"subagent".to_owned()),
        "{tool_names:?}"
    );
}

// ---------------------------------------------------------------------------
// Builtin replacement: exact upstream warning text
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn builtin_subagent_replacement_omits_with_the_exact_upstream_warning() {
    let temp = TempDir::new("replace-subagent");
    let source = r#"
        export default function (pi) {
            pi.registerTool({
                name: "subagent",
                description: "js replacement for the subagent builtin",
                parameters: { type: "object" },
                execute: async () => ({ content: [{ type: "text", text: "from-js" }] }),
            });
        }
    "#;
    let runtime = load_runtime(&temp, &[("replacer.ts", source)]).await;
    runtime.set_path_plan(vec![
        PathBuf::from("builtin:subagent"),
        temp.0.join("replacer.ts"),
    ]);
    let host = PluginHost::build(Some(runtime), Vec::new());

    // The builtin is omitted from the dispatch list.
    let paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert_eq!(
        paths,
        vec!["<inline:js-extensions>".to_owned()],
        "{paths:?}"
    );

    // The warning is the upstream template verbatim (upstream
    // `resource-loader.ts`: `Extension ${path} registers ${kind} \`${name}\`,
    // so built-in extension \`${builtin}\` was not loaded. ...`).
    let diagnostics = host.diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].kind, pir::plugins::DiagnosticKind::Warning);
    assert_eq!(diagnostics[0].path, "builtin:subagent");
    assert_eq!(
        diagnostics[0].message,
        "Extension <inline:js-extensions> registers tool `subagent`, so built-in extension `subagent` was not loaded. To use `subagent`, run `pi config` and make sure it is enabled under Built-in extensions, then disable or remove the existing extension. We recommend only having one or the other loaded at a time."
    );

    // The surviving JS tool is callable through the host.
    let result = host
        .call_tool("subagent", "call-1", &json!({}))
        .await
        .expect("js replacement executes");
    assert_eq!(
        result.content.first().map(|item| item.text.as_str()),
        Some("from-js"),
        "{result:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_mcp_replacement_via_the_mcp_command_omits_with_the_upstream_warning() {
    let temp = TempDir::new("replace-mcp");
    // Upstream's mcp extension claims the `/mcp` command
    // (`pi.registerCommand("mcp", ...)`), so a JS extension registering the
    // same command omits the builtin.
    let source = r#"
        export default function (pi) {
            pi.registerCommand("mcp", { handler: async () => {} });
        }
    "#;
    let runtime = load_runtime(&temp, &[("replacer.ts", source)]).await;
    runtime.set_path_plan(vec![
        PathBuf::from("builtin:mcp"),
        temp.0.join("replacer.ts"),
    ]);
    let host = PluginHost::build(Some(runtime), Vec::new());

    let paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert_eq!(paths, vec!["<inline:js-extensions>".to_owned()]);
    // The omitted mcp builtin leaves NO manager on the host.
    assert!(host.mcp_manager().is_none());
    let diagnostics = host.diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0]
            .message
            .starts_with("Extension <inline:js-extensions> registers command `/mcp`, so built-in extension `mcp` was not loaded."),
        "{}",
        diagnostics[0].message
    );

    // A JS extension registering a TOOL named "mcp" (upstream's mcp
    // extension registers no such tool) does NOT omit the builtin: both
    // stay loaded, no false replacement.
    let temp2 = TempDir::new("replace-mcp-tool");
    let source2 = r#"
        export default function (pi) {
            pi.registerTool({
                name: "mcp",
                description: "a tool that happens to be named mcp",
                parameters: { type: "object" },
                execute: async () => ({ content: [{ type: "text", text: "from-js" }] }),
            });
        }
    "#;
    let runtime2 = load_runtime(&temp2, &[("replacer.ts", source2)]).await;
    runtime2.set_path_plan(vec![
        PathBuf::from("builtin:mcp"),
        temp2.0.join("replacer.ts"),
    ]);
    let host2 = PluginHost::build(Some(runtime2), Vec::new());
    let paths2: Vec<String> = host2
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert_eq!(
        paths2,
        vec![
            "builtin:mcp".to_owned(),
            "<inline:js-extensions>".to_owned()
        ],
        "{paths2:?}"
    );
    assert!(host2.diagnostics().is_empty(), "{:?}", host2.diagnostics());
}
