//! Unit-level MCP tests: JSON-RPC message model, SSE parser, tool naming and
//! matchers, exposure resolution, server registry change events, config
//! loading (global + trust-gated project), result conversion, and the typed
//! CLI errors. Client/transport/plugin flows live in `tests/mcp_integration.rs`.
//! The built-in `sd` MCP server's replace engine (`pir mcp-sd`) is pinned
//! here; its end-to-end stdio surface lives in `tests/mcp_integration.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use pir::ai::auth::resolve::{refresh_provider_token_after_rejection, resolve_provider_token};
use pir::ai::auth::{FileAuthStorage, StoredCredential};
use pir::ai::error::AiError;
use pir::config::mcp::{load_mcp_configs, parse_mcp_config, McpExposure, McpServerConfig};
use pir::mcp::error::McpError;
use pir::mcp::jsonrpc::{JsonRpcMessage, RequestId};
use pir::mcp::manager::{DiscoveredTool, McpManager};
use pir::mcp::protocol::{CallToolResult, ContentBlock, McpTool, ResourceContents};
use pir::mcp::registry::{
    assign_mcp_tool_names, create_mcp_tool_name, get_mcp_tool_exposure, is_mcp_tool_name,
    mcp_namespace, tool_name_matcher, McpServerEntry, McpServerRegistry, McpServerSource,
};
use pir::mcp::server::sd::{replace_text, run as sd_run, SdRequest, SdServer};
use pir::mcp::sse::{is_json_rpc_event, SseParser};
use pir::mcp::transport::DEFAULT_MAX_MESSAGE_BYTES;
use pir::plugins::builtin::{convert_mcp_result, McpToolBridge};
use pir::plugins::services::{
    provider_auth_hook_registry, AuthPromptContext, ProviderAuth, ProviderAuthError,
    ProviderAuthHooks, ServiceFuture,
};

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

#[test]
fn jsonrpc_request_response_round_trip() {
    let id = RequestId::Number(7);
    let request = JsonRpcMessage::request(&id, "tools/list", json!({"cursor": "a"}));
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["method"], "tools/list");
    assert_eq!(request["params"]["cursor"], "a");

    let parsed = JsonRpcMessage::parse(request).expect("parse request");
    match parsed {
        JsonRpcMessage::Request { id, method, params } => {
            assert_eq!(id, RequestId::Number(7));
            assert_eq!(method, "tools/list");
            assert_eq!(params["cursor"], "a");
        }
        other => panic!("unexpected message: {other:?}"),
    }

    let response = JsonRpcMessage::response_ok(&id, json!({"tools": []}));
    match JsonRpcMessage::parse(response).expect("parse response") {
        JsonRpcMessage::Success { id, result } => {
            assert_eq!(id, RequestId::Number(7));
            assert_eq!(result["tools"], json!([]));
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn jsonrpc_error_response_and_notifications() {
    let id = RequestId::String("abc".to_owned());
    let error = JsonRpcMessage::response_error(&id, -32601, "no such method");
    match JsonRpcMessage::parse(error).expect("parse") {
        JsonRpcMessage::Failure {
            id,
            code,
            message,
            data,
        } => {
            assert_eq!(id, RequestId::String("abc".to_owned()));
            assert_eq!(code, -32601);
            assert_eq!(message, "no such method");
            assert!(data.is_none());
        }
        other => panic!("unexpected message: {other:?}"),
    }

    let notification = JsonRpcMessage::notification("notifications/initialized", Value::Null);
    match JsonRpcMessage::parse(notification).expect("parse") {
        JsonRpcMessage::Notification { method, .. } => {
            assert_eq!(method, "notifications/initialized");
        }
        other => panic!("unexpected message: {other:?}"),
    }

    // Not JSON-RPC.
    assert!(JsonRpcMessage::parse(json!({"nope": true})).is_err());
    assert!(JsonRpcMessage::parse(json!(42)).is_err());
}

#[test]
fn jsonrpc_parse_line_skips_noise() {
    let parse_line = |text: &str| -> Option<JsonRpcMessage> {
        let value = serde_json::from_str::<Value>(text).ok()?;
        JsonRpcMessage::parse(value).ok()
    };
    assert!(parse_line("not json").is_none());
    assert!(parse_line("").is_none());
    let request = JsonRpcMessage::request(&RequestId::Number(1), "ping", Value::Null);
    assert!(parse_line(&request.to_string()).is_some());
}

// ---------------------------------------------------------------------------
// SSE parser
// ---------------------------------------------------------------------------

#[test]
fn sse_parser_joins_data_lines_and_dispatches_on_blank() {
    let mut parser = SseParser::new(1024);
    assert_eq!(parser.feed_line("event: message").expect("line"), None);
    assert_eq!(parser.feed_line("id: 42").expect("line"), None);
    assert_eq!(parser.feed_line("data: first").expect("line"), None);
    assert_eq!(parser.feed_line("data: second").expect("line"), None);
    let event = parser.feed_line("").expect("line").expect("dispatched");
    assert_eq!(event.event.as_deref(), Some("message"));
    assert_eq!(event.id.as_deref(), Some("42"));
    assert_eq!(event.data, "first\nsecond");
    assert!(is_json_rpc_event(&event));

    // Comments and unknown fields are ignored; an event without any `data`
    // line dispatches nothing (resumption priming rides the `id` callback).
    assert_eq!(parser.feed_line(": keep-alive").expect("line"), None);
    assert_eq!(parser.feed_line("id: 43").expect("line"), None);
    assert_eq!(parser.feed_line("").expect("line"), None);
    assert_eq!(parser.feed_line("event: ping").expect("line"), None);
    assert_eq!(parser.feed_line("").expect("line"), None);
}

#[test]
fn sse_parser_enforces_event_cap() {
    let mut parser = SseParser::new(8);
    let result = parser.feed_line("data: 0123456789");
    assert!(
        matches!(result, Err(McpError::MessageTooLarge { limit: 8 })),
        "expected MessageTooLarge, got {result:?}"
    );
}

#[test]
fn sse_parser_crlf_and_finish() {
    let mut parser = SseParser::new(1024);
    assert_eq!(parser.feed_line("data: hello\r").expect("line"), None);
    let event = parser.feed_line("\r").expect("line").expect("dispatched");
    assert_eq!(event.data, "hello");
    // finish flushes a trailing event without a blank line.
    assert_eq!(parser.feed_line("data: tail").expect("line"), None);
    assert_eq!(parser.finish().map(|event| event.data), Some("tail".into()));
    assert_eq!(DEFAULT_MAX_MESSAGE_BYTES, 1024 * 1024);
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

#[test]
fn tool_name_sanitizes_and_shortens() {
    assert_eq!(
        create_mcp_tool_name("web", "search", |_| false),
        "mcp__web__search"
    );
    assert_eq!(
        create_mcp_tool_name("a-b", "c.d", |_| false),
        "mcp__a_b__c_d"
    );
    // Taken names get a hash suffix of deterministic shape (short names keep
    // their prefix; the suffix only extends, never fills to the cap).
    let taken = create_mcp_tool_name("web", "search", |name| name == "mcp__web__search");
    assert_ne!(taken, "mcp__web__search");
    assert!(taken.starts_with("mcp__web__search_"));
    assert_eq!(taken.len(), 25); // prefix + '_' + 8 hex chars
                                 // A very long tool name is shortened to the cap.
    let long = create_mcp_tool_name("web", &"t".repeat(200), |_| false);
    assert_eq!(long.len(), 64);
}

#[test]
fn tool_name_assignment_deconflicts_collisions() {
    // `a-b` and `a_b` sanitize to the same name: both get hash suffixes.
    let names = assign_mcp_tool_names("srv", &["a-b".into(), "a_b".into()]);
    assert_ne!(names[0], names[1]);
    assert!(
        names.iter().all(|name| name.starts_with("mcp__srv__a_b_")),
        "both colliding tools get hash suffixes: {names:?}"
    );
    // Unique names keep the plain form.
    let names = assign_mcp_tool_names("srv", &["one".into(), "two".into()]);
    assert_eq!(names, vec!["mcp__srv__one", "mcp__srv__two"]);
}

#[test]
fn mcp_tool_name_predicates() {
    assert!(is_mcp_tool_name("mcp__srv__tool"));
    assert!(is_mcp_tool_name("list_mcp_resources"));
    assert!(is_mcp_tool_name("read_mcp_resource"));
    assert!(!is_mcp_tool_name("read"));
    assert_eq!(mcp_namespace("my-server"), "mcp__my_server");
}

#[test]
fn tool_name_matcher_exact_and_patterns() {
    let matcher = tool_name_matcher(&["read".into(), "mcp__*".into(), "fs_*".into()]);
    assert!(matcher.matches("read"));
    assert!(matcher.matches("mcp__anything__here"));
    assert!(matcher.matches("fs_read"));
    assert!(!matcher.matches("write"));
}

#[test]
fn exposure_resolution_exact_then_pattern_then_default() {
    use pir::config::mcp::{McpServerConfigBase, McpStdioServerConfig};
    let config = McpServerConfig::Stdio(McpStdioServerConfig {
        base: McpServerConfigBase {
            exposure: Some(McpExposure::Codemode),
            tool_exposure: Some(BTreeMap::from([
                ("safe".to_owned(), McpExposure::Direct),
                ("gone_*".to_owned(), McpExposure::Hidden),
            ])),
            ..Default::default()
        },
        command: "true".into(),
        ..Default::default()
    });
    assert_eq!(get_mcp_tool_exposure(&config, "safe"), McpExposure::Direct);
    assert_eq!(
        get_mcp_tool_exposure(&config, "gone_tool"),
        McpExposure::Hidden
    );
    assert_eq!(
        get_mcp_tool_exposure(&config, "other"),
        McpExposure::Codemode
    );
}

// ---------------------------------------------------------------------------
// Registry change events
// ---------------------------------------------------------------------------

#[test]
fn registry_queues_changes_on_register_and_unregister() {
    let registry = McpServerRegistry::new();
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": "echo",
        "args": ["hi"],
    }))
    .expect("config");
    registry.register(McpServerEntry {
        name: "echo".into(),
        config: config.clone(),
        source: McpServerSource::GlobalConfig(std::path::PathBuf::from("global.json")),
    });
    registry.register(McpServerEntry {
        name: "echo".into(),
        config,
        source: McpServerSource::Extension("ext.ts".into()),
    });
    registry.unregister("missing");
    let changes = registry.drain_changes();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].action, "registered");
    assert_eq!(changes[0].server, "echo");
    assert_eq!(changes[1].action, "replaced");
    // Drained: the queue is empty afterwards.
    assert!(registry.drain_changes().is_empty());
    registry.unregister("echo");
    let changes = registry.drain_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].action, "unregistered");
    // The change payload serializes to the camelCase wire shape.
    let wire = serde_json::to_value(&changes[0]).expect("serialize");
    assert_eq!(wire["action"], "unregistered");
    assert_eq!(wire["server"], "echo");
}

// ---------------------------------------------------------------------------
// Config precedence + extension unregistration (registerMcpServer)
// ---------------------------------------------------------------------------

fn extension_entry(name: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.to_owned(),
        config: serde_json::from_value(json!({ "command": "ext-command" })).expect("config"),
        source: McpServerSource::Extension("ext.ts".into()),
    }
}

#[test]
fn config_entries_win_over_extension_registrations() {
    let registry = McpServerRegistry::new();
    registry.register(McpServerEntry {
        name: "shared".into(),
        config: serde_json::from_value(json!({ "command": "from-mcp-json" })).expect("config"),
        source: McpServerSource::GlobalConfig(std::path::PathBuf::from("mcp.json")),
    });
    // An extension registering the same name is skipped and recorded as
    // overridden (upstream: a server in mcp.json takes precedence).
    assert!(
        !registry.register_extension(extension_entry("shared")),
        "the extension registration is skipped"
    );
    let entry = registry.get("shared").expect("entry");
    assert_eq!(
        serde_json::to_value(&entry.config).expect("serialize")["command"],
        json!("from-mcp-json"),
        "the mcp.json config stays in place"
    );
    assert!(matches!(entry.source, McpServerSource::GlobalConfig(_)));
    let changes = registry.drain_changes();
    assert!(
        changes
            .iter()
            .any(|change| change.action == "overridden" && change.server == "shared"),
        "the override is recorded: {changes:?}"
    );

    // Names without config-file entries register (and re-register replaces).
    assert!(registry.register_extension(extension_entry("fresh")));
    assert!(registry.get("fresh").is_some());
    assert!(registry.register_extension(extension_entry("fresh")));
    let actions: Vec<String> = registry
        .drain_changes()
        .into_iter()
        .map(|change| change.action)
        .collect();
    assert_eq!(actions, vec!["registered", "replaced"]);
}

#[test]
fn extension_servers_unregister_when_removed_from_the_def_list() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mcp-units-plugin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp agent dir");
    std::env::set_var("PIR_AGENT_DIR", &dir);
    let plugin = pir::plugins::builtin::McpPlugin::new();

    let def = json!({ "name": "ext-srv", "config": { "command": "ext-command" } });
    plugin.apply_js_server_defs(std::slice::from_ref(&def));
    assert!(
        plugin.manager().registry().get("ext-srv").is_some(),
        "the extension server registers"
    );

    // A later rebuild without the def unregisters the live entry.
    plugin.apply_js_server_defs(&[]);
    assert!(
        plugin.manager().registry().get("ext-srv").is_none(),
        "the removed extension server is unregistered"
    );
    let changes: Vec<String> = plugin
        .manager()
        .registry()
        .drain_changes()
        .into_iter()
        .map(|change| format!("{}:{}", change.action, change.server))
        .collect();
    assert!(
        changes
            .iter()
            .any(|change| change == "unregistered:ext-srv"),
        "the unregistered change is emitted: {changes:?}"
    );

    // Re-registering on the next rebuild works again.
    plugin.apply_js_server_defs(&[def]);
    assert!(plugin.manager().registry().get("ext-srv").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_js_server_defs_respects_config_precedence() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mcp-units-precedence-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp agent dir");
    std::env::set_var("PIR_AGENT_DIR", &dir);
    let plugin = pir::plugins::builtin::McpPlugin::new();
    plugin.manager().registry().register(McpServerEntry {
        name: "shared".into(),
        config: serde_json::from_value(json!({ "command": "from-mcp-json" })).expect("config"),
        source: McpServerSource::GlobalConfig(std::path::PathBuf::from("mcp.json")),
    });
    plugin.apply_js_server_defs(&[json!({
        "name": "shared",
        "config": { "command": "from-extension" },
    })]);
    let entry = plugin.manager().registry().get("shared").expect("entry");
    assert_eq!(
        serde_json::to_value(&entry.config).expect("serialize")["command"],
        json!("from-mcp-json"),
        "mcp.json wins over registerMcpServer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Config loading (global + trust-gated project)
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-units-{label}-{}-{}",
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

#[test]
fn parse_mcp_config_drops_invalid_entries() {
    let (servers, errors) = parse_mcp_config(
        r#"{
            "good-stdio": { "command": "node", "args": ["server.js"] },
            "good-http": { "type": "http", "url": "https://example.com/mcp" },
            "bad-sse": { "type": "sse", "url": "https://example.com/sse" },
            "bad-name": { "command": "" },
            "neither": {}
        }"#,
    );
    assert!(servers.contains_key("good-stdio"));
    assert!(servers.contains_key("good-http"));
    assert_eq!(servers.len(), 2, "invalid entries are dropped: {servers:?}");
    assert_eq!(errors.len(), 3);
}

#[test]
fn load_mcp_configs_gates_project_on_trust() {
    let dir = TempDir::new("trust");
    let agent_dir = dir.0.join("agent");
    let project = dir.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(project.join(".pi")).expect("project dir");
    std::fs::write(
        pir::config::mcp::global_mcp_path(&agent_dir),
        r#"{ "global-server": { "command": "global" } }"#,
    )
    .expect("write global");
    std::fs::write(
        pir::config::mcp::project_mcp_path(&project),
        r#"{ "project-server": { "command": "project" } }"#,
    )
    .expect("write project");

    let untrusted = load_mcp_configs(&agent_dir, &project, false);
    assert_eq!(untrusted.len(), 1, "project file is skipped when untrusted");
    assert!(untrusted[0].servers.contains_key("global-server"));

    let trusted = load_mcp_configs(&agent_dir, &project, true);
    assert_eq!(trusted.len(), 2);
    let all: Vec<String> = trusted
        .iter()
        .flat_map(|file| file.servers.keys().cloned())
        .collect();
    assert!(all.contains(&"global-server".to_owned()));
    assert!(all.contains(&"project-server".to_owned()));
}

// ---------------------------------------------------------------------------
// Project `mcp.json` overrides (upstream extensions/mcp/config.ts:75-139)
// ---------------------------------------------------------------------------

/// Write one `mcp.json` + its project counterpart and return the loaded
/// documents (trusted project).
fn load_pair(label: &str, global: &str, project: &str) -> Vec<pir::config::mcp::LoadedMcpConfig> {
    let dir = TempDir::new(label);
    let agent_dir = dir.0.join("agent");
    let project_dir = dir.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(project_dir.join(".pi")).expect("project dir");
    std::fs::write(pir::config::mcp::global_mcp_path(&agent_dir), global).expect("write global");
    std::fs::write(pir::config::mcp::project_mcp_path(&project_dir), project)
        .expect("write project");
    let loaded = load_mcp_configs(&agent_dir, &project_dir, true);
    // Keep the temp dir alive for the length of the call: the documents are
    // already in memory, so early removal would be safe, but `dir` must not be
    // dropped before the files are read above.
    drop(dir);
    loaded
}

/// The last (effective) definition of `name` across the loaded files.
fn effective_entry<'a>(
    loaded: &'a [pir::config::mcp::LoadedMcpConfig],
    name: &str,
) -> Option<&'a McpServerConfig> {
    loaded.iter().rev().find_map(|file| file.servers.get(name))
}

#[test]
fn project_override_merges_enabled_exposure_and_tool_exposure() {
    let loaded = load_pair(
        "override-merge",
        r#"{
            "mcpServers": {
                "internal-tools": {
                    "command": "run-tools",
                    "args": ["--serve"],
                    "enabled": true,
                    "exposure": "codemode",
                    "toolExposure": { "read": "hidden" }
                }
            }
        }"#,
        r#"{
            "mcpServers": {
                "internal-tools": {
                    "enabled": false,
                    "exposure": "hidden",
                    "toolExposure": { "write": "deferred" }
                }
            }
        }"#,
    );
    let errors: Vec<&String> = loaded.iter().flat_map(|file| file.errors.iter()).collect();
    assert!(errors.is_empty(), "the override is valid: {errors:?}");
    let entry = effective_entry(&loaded, "internal-tools").expect("merged entry");
    assert!(
        !entry.is_enabled(),
        "the project turns the global server off"
    );
    assert_eq!(entry.effective_exposure(true), McpExposure::Hidden);
    // The rest of the global entry is kept, credentials included.
    let McpServerConfig::Stdio(stdio) = entry else {
        panic!("the base transport is kept: {entry:?}");
    };
    assert_eq!(stdio.command, "run-tools");
    assert_eq!(stdio.args.as_deref(), Some(&["--serve".to_owned()][..]));
    // `toolExposure` is replaced wholesale (upstream's object spread).
    assert_eq!(
        entry.base().explicit_tool_exposure("write"),
        Some(McpExposure::Deferred)
    );
    assert_eq!(entry.base().explicit_tool_exposure("read"), None);
}

#[test]
fn project_override_with_other_keys_is_rejected() {
    let loaded = load_pair(
        "override-keys",
        r#"{ "mcpServers": { "internal-tools": { "command": "run-tools" } } }"#,
        r#"{
            "mcpServers": {
                "internal-tools": { "enabled": false, "timeout": 5, "headers": { "X-Token": "t" } },
                "unknown-base": { "enabled": false }
            }
        }"#,
    );
    let project = loaded.last().expect("project document");
    assert!(project.servers.is_empty(), "no override was accepted");
    assert!(
        project
            .errors
            .iter()
            .any(|error| error
                == "server \"internal-tools\": an override can only set enabled, exposure, toolExposure"),
        "the extra keys are reported: {:?}",
        project.errors
    );
    assert!(
        project.errors.iter().any(|error| error
            == "server \"unknown-base\" needs \"command\" or \"url\", or a global server to override"),
        "an override without a base is reported: {:?}",
        project.errors
    );
    // The global definition is untouched.
    assert!(loaded[0].servers.contains_key("internal-tools"));
}

#[test]
fn project_auth_is_rejected_but_global_auth_is_allowed() {
    let auth_entry = r#"{
        "url": "https://mcp.example.com/mcp",
        "auth": { "provider": "fake-provider" }
    }"#;
    let global = format!(r#"{{ "mcpServers": {{ "docs": {auth_entry} }} }}"#);
    let project = format!(r#"{{ "mcpServers": {{ "project-docs": {auth_entry} }} }}"#);
    let loaded = load_pair("project-auth", &global, &project);
    assert!(
        loaded[0].servers.contains_key("docs"),
        "the global file may delegate its token to a provider"
    );
    let project_doc = loaded.last().expect("project document");
    assert!(
        !project_doc.servers.contains_key("project-docs"),
        "a project entry cannot point a credential at a server"
    );
    assert!(
        project_doc
            .errors
            .iter()
            .any(|error| error
                == "server \"project-docs\": auth is only allowed in the global mcp.json"),
        "the project auth is reported: {:?}",
        project_doc.errors
    );
}

#[test]
fn server_names_conflicting_by_dash_underscore_are_rejected() {
    // Within one file: `a-b` and `a_b` would share `mcp__a_b`.
    let (servers, errors) = parse_mcp_config(
        r#"{
            "mcpServers": {
                "a-b": { "command": "one" },
                "a_b": { "command": "two" }
            }
        }"#,
    );
    assert_eq!(servers.len(), 1, "the later entry is dropped: {servers:?}");
    assert!(servers.contains_key("a-b"));
    assert!(
        errors
            .iter()
            .any(|error| error == "server \"a_b\" conflicts with \"a-b\""),
        "the namespace conflict is reported: {errors:?}"
    );

    // Across files: a project entry cannot take a namespace the global file
    // already owns.
    let loaded = load_pair(
        "namespace-conflict",
        r#"{ "mcpServers": { "remote-tools": { "command": "global" } } }"#,
        r#"{ "mcpServers": { "remote_tools": { "command": "project" } } }"#,
    );
    let project_doc = loaded.last().expect("project document");
    assert!(!project_doc.servers.contains_key("remote_tools"));
    assert!(
        project_doc
            .errors
            .iter()
            .any(|error| error == "server \"remote_tools\" conflicts with \"remote-tools\""),
        "the cross-file conflict is reported: {:?}",
        project_doc.errors
    );
}

#[test]
fn extension_registration_is_overridden_by_a_config_namespace() {
    let registry = McpServerRegistry::new();
    registry.register(McpServerEntry {
        name: "remote-tools".into(),
        config: serde_json::from_value(json!({ "command": "from-mcp-json" })).expect("config"),
        source: McpServerSource::GlobalConfig(PathBuf::from("mcp.json")),
    });
    // `remote_tools` shares `mcp__remote_tools` with the configured server.
    assert!(
        !registry.register_extension(extension_entry("remote_tools")),
        "the colliding extension registration is skipped"
    );
    assert!(registry.get("remote-tools").is_some());
    assert!(registry.get("remote_tools").is_none());
    let overridden = registry
        .drain_changes()
        .into_iter()
        .find(|change| change.action == "overridden")
        .expect("an overridden change is queued");
    assert_eq!(overridden.server, "remote_tools");
}

/// A native provider-auth hook that only refreshes: `Unavailable` for login,
/// and a fixed (or failing) refresh result, counting the calls.
struct RefreshOnlyHooks {
    calls: Arc<AtomicUsize>,
    result: std::sync::Mutex<Result<ProviderAuth, ProviderAuthError>>,
}

impl RefreshOnlyHooks {
    fn new(calls: Arc<AtomicUsize>, result: Result<ProviderAuth, ProviderAuthError>) -> Arc<Self> {
        Arc::new(Self {
            calls,
            result: std::sync::Mutex::new(result),
        })
    }
}

impl ProviderAuthHooks for RefreshOnlyHooks {
    fn login<'a>(
        &'a self,
        _context: AuthPromptContext<'a>,
    ) -> ServiceFuture<'a, Result<ProviderAuth, ProviderAuthError>> {
        Box::pin(async { Err(ProviderAuthError::Unavailable) })
    }

    fn refresh(
        &self,
        _current: &ProviderAuth,
    ) -> ServiceFuture<'static, Result<ProviderAuth, ProviderAuthError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { result })
    }
}

/// Unregisters a test hook even when an assertion panics.
struct HookGuard {
    provider: String,
    hooks: Arc<dyn ProviderAuthHooks>,
}

impl HookGuard {
    fn install(provider: &str, hooks: Arc<dyn ProviderAuthHooks>) -> Self {
        provider_auth_hook_registry().register(provider, Arc::clone(&hooks));
        Self {
            provider: provider.to_owned(),
            hooks,
        }
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        provider_auth_hook_registry().unregister_if(&self.provider, &self.hooks);
    }
}

fn stored_oauth(dir: &Path, provider: &str) -> StoredCredential {
    FileAuthStorage::in_agent_dir(dir)
        .read_stored(provider)
        .expect("read auth.json")
        .expect("stored credential")
}

#[tokio::test]
async fn auth_provider_servers_name_the_provider_login_and_read_the_token() {
    let config: McpServerConfig = serde_json::from_value(json!({
        "url": "https://mcp.example.com/mcp",
        "auth": { "provider": "fake-provider" }
    }))
    .expect("config");
    assert_eq!(
        pir::mcp::oauth::sign_in_required_message_for(&config, "docs"),
        "MCP server \"docs\" requires sign-in. Run /login fake-provider to sign in."
    );
    assert_eq!(
        McpError::ProviderLoginRequired {
            server: "docs".to_owned(),
            provider: "fake-provider".to_owned(),
        }
        .to_string(),
        "MCP server \"docs\" requires sign-in. Run /login fake-provider to sign in."
    );

    // The token comes from the agent directory's `auth.json`, read fresh each
    // call.
    let dir = TempDir::new("provider-token");
    std::fs::write(
        dir.0.join("auth.json"),
        r#"{
            "fake-provider": { "type": "api_key", "key": "sk-test" },
            "oauth-provider": { "type": "oauth", "refresh": "r", "access": "access-test", "expires": 0 }
        }"#,
    )
    .expect("write auth.json");
    assert_eq!(
        pir::mcp::manager::provider_token_in(&dir.0, "fake-provider")
            .await
            .as_deref(),
        Some("sk-test")
    );
    // A stored OAuth credential without a matching flow is unconfigured: the
    // normal chain never falls back to ambient sources for it (upstream
    // resolve.ts), so its bare access token is not guessed here either.
    assert_eq!(
        pir::mcp::manager::provider_token_in(&dir.0, "oauth-provider").await,
        None
    );
    assert_eq!(
        pir::mcp::manager::provider_token_in(&dir.0, "unknown-provider").await,
        None
    );
    assert_eq!(
        pir::mcp::manager::provider_token_in(Path::new(""), "fake-provider").await,
        None,
        "a rootless manager has no auth store"
    );
}

#[tokio::test]
async fn provider_token_falls_back_to_the_models_json_provider_key() {
    let dir = TempDir::new("provider-token-models-json");
    std::fs::write(
        dir.0.join("models.json"),
        r#"{
            "providers": {
                "models-json-provider": { "apiKey": "sk-from-models-json", "api": "openai-chat" }
            }
        }"#,
    )
    .expect("write models.json");

    assert_eq!(
        resolve_provider_token(&dir.0, "models-json-provider")
            .await
            .expect("resolve")
            .as_deref(),
        Some("sk-from-models-json"),
        "models.json supplies the provider key when nothing is stored"
    );

    // A stored api-key credential owns the provider: models.json does not
    // shadow it.
    std::fs::write(
        dir.0.join("auth.json"),
        r#"{ "models-json-provider": { "type": "api_key", "key": "sk-stored" } }"#,
    )
    .expect("write auth.json");
    assert_eq!(
        resolve_provider_token(&dir.0, "models-json-provider")
            .await
            .expect("resolve")
            .as_deref(),
        Some("sk-stored")
    );

    // A provider absent from both sources stays unconfigured.
    assert_eq!(
        resolve_provider_token(&dir.0, "nowhere-provider")
            .await
            .expect("resolve"),
        None
    );
}

#[tokio::test]
async fn provider_token_refreshes_an_expiring_oauth_credential() {
    const PROVIDER: &str = "mcp-refresh-provider";
    let dir = TempDir::new("provider-token-refresh");
    let expires = pir::ai::util::now_ms() as i64 + 3_600_000;
    std::fs::write(
        dir.0.join("auth.json"),
        format!(
            r#"{{ "{PROVIDER}": {{ "type": "oauth", "refresh": "old-refresh", "access": "old-access", "expires": 0 }} }}"#
        ),
    )
    .expect("write auth.json");
    let calls = Arc::new(AtomicUsize::new(0));
    let _guard = HookGuard::install(
        PROVIDER,
        RefreshOnlyHooks::new(
            calls.clone(),
            Ok(ProviderAuth::oauth(
                "fresh-access",
                "fresh-refresh",
                expires,
            )),
        ),
    );

    assert_eq!(
        resolve_provider_token(&dir.0, PROVIDER)
            .await
            .expect("resolve")
            .as_deref(),
        Some("fresh-access"),
        "the expiring access token is replaced through the locked refresh"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh");
    match stored_oauth(&dir.0, PROVIDER) {
        StoredCredential::OAuth(credential) => {
            assert_eq!(credential.access, "fresh-access");
            assert_eq!(credential.refresh, "fresh-refresh");
        }
        other => panic!("expected a stored OAuth credential, got {other:?}"),
    }
}

#[tokio::test]
async fn provider_token_refresh_failure_after_a_401_is_an_auth_error() {
    const PROVIDER: &str = "mcp-failing-refresh-provider";
    let dir = TempDir::new("provider-token-failing-refresh");
    let expires = pir::ai::util::now_ms() as i64 + 3_600_000;
    std::fs::write(
        dir.0.join("auth.json"),
        format!(
            r#"{{ "{PROVIDER}": {{ "type": "oauth", "refresh": "old-refresh", "access": "rejected-access", "expires": {expires} }} }}"#
        ),
    )
    .expect("write auth.json");
    let calls = Arc::new(AtomicUsize::new(0));
    let _guard = HookGuard::install(
        PROVIDER,
        RefreshOnlyHooks::new(
            calls.clone(),
            Err(ProviderAuthError::Failed("invalid_grant".to_owned())),
        ),
    );

    let error = refresh_provider_token_after_rejection(&dir.0, PROVIDER, Some("rejected-access"))
        .await
        .expect_err("the refresh failure must reach the caller");
    assert!(
        matches!(error, AiError::Authentication(_)),
        "a failed refresh is an auth error: {error:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh attempt");
}

// ---------------------------------------------------------------------------
// Stdio process-tree teardown (E6)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn unix_pid_alive(pid: u32) -> bool {
    // `ps` reports a zombie (`Z`) while a killed unreaped process still
    // answers `kill -0`; treat it as gone.
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let stat = String::from_utf8_lossy(&output.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }
        _ => false,
    }
}

/// A SIGTERM-ignoring grandchild in the server's process group must not
/// survive a transport close: `terminate_process_tree` escalates to SIGKILL
/// after the grace (the old implementation sent SIGTERM only).
#[cfg(unix)]
#[test]
fn stdio_tree_kill_escalates_to_sigkill_for_a_sigterm_ignoring_grandchild() {
    use std::os::unix::process::CommandExt as _;

    let pid_file = std::env::temp_dir().join(format!(
        "pir-mcp-sigkill-{}-{}.pid",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let script = format!(
        "trap '' TERM; sh -c 'trap \"\" TERM; echo $$ > {}; while :; do sleep 1; done' & wait",
        pid_file.display()
    );
    let mut leader = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Own process group: the same shape the stdio transport spawns.
        .process_group(0)
        .spawn()
        .expect("spawn process-group leader");
    let leader_pid = leader.id();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let grandchild = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                break pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the grandchild never reported its pid"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(unix_pid_alive(grandchild), "the grandchild starts alive");

    pir::mcp::stdio::terminate_process_tree(leader_pid);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while unix_pid_alive(grandchild) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !unix_pid_alive(grandchild),
        "SIGTERM was ignored; the force phase must SIGKILL the group"
    );
    let _ = leader.wait();
    let _ = std::fs::remove_file(&pid_file);
}

// ---------------------------------------------------------------------------
// Result conversion
// ---------------------------------------------------------------------------

#[test]
fn convert_mcp_result_flattens_content_blocks() {
    let result = CallToolResult {
        content: vec![
            ContentBlock {
                kind: "text".into(),
                text: Some("hello".into()),
                ..Default::default()
            },
            ContentBlock {
                kind: "resource_link".into(),
                uri: Some("mock://res/1".into()),
                name: Some("one".into()),
                mime_type: Some("text/plain".into()),
                ..Default::default()
            },
            ContentBlock {
                kind: "resource".into(),
                resource: Some(ResourceContents {
                    uri: "mock://res/2".into(),
                    mime_type: Some("text/plain".into()),
                    text: None,
                    blob: Some("d29ybGQ=".into()), // "world"
                }),
                ..Default::default()
            },
        ],
        structured_content: Some(json!({"answer": 42})),
        is_error: None,
        meta: None,
    };
    let tool_result = convert_mcp_result("srv", "read", result);
    assert!(tool_result.content.contains("hello"));
    assert!(tool_result
        .content
        .contains("[Resource mock://res/1 \"one\" (text/plain)]"));
    assert!(tool_result.content.contains("world"));
    assert!(!tool_result.is_error);
    assert_eq!(
        tool_result.details.as_ref().expect("details")["server"],
        "srv"
    );
    assert_eq!(
        tool_result.details.as_ref().expect("details")["tool"],
        "read"
    );
    let structured = tool_result.structured_content.expect("structured");
    assert_eq!(structured["structuredContent"]["answer"], 42);
    assert_eq!(structured["isError"], false);
}

#[test]
fn convert_mcp_result_maps_is_error_and_structured_fallback() {
    let result = CallToolResult {
        content: Vec::new(),
        structured_content: Some(json!({"reason": "nope"})),
        is_error: Some(true),
        meta: None,
    };
    let tool_result = convert_mcp_result("srv", "boom", result);
    assert!(tool_result.is_error);
    assert!(
        tool_result.content.contains("nope"),
        "structured content becomes the text when no blocks: {}",
        tool_result.content
    );
}

#[test]
fn convert_mcp_result_truncates_middle() {
    let long = "x".repeat(50_000);
    let result = CallToolResult {
        content: vec![ContentBlock {
            kind: "text".into(),
            text: Some(long),
            ..Default::default()
        }],
        structured_content: None,
        is_error: None,
        meta: None,
    };
    let tool_result = convert_mcp_result("srv", "big", result);
    assert!(tool_result.content.len() < 50_000);
    assert!(tool_result.content.contains("truncated"));
    assert!(tool_result.content.starts_with("Warning: truncated output"));
}

/// Image blocks and image-MIME embedded resources reach the model as real
/// `ImageContent` (not a "[image: mime]" placeholder with the bytes dropped);
/// non-vision adapters already downgrade image content themselves.
#[test]
fn mcp_image_blocks_and_image_resources_reach_the_model_as_images() {
    use base64::Engine as _;

    let png = base64::engine::general_purpose::STANDARD.encode(b"png-bytes");
    let jpeg = base64::engine::general_purpose::STANDARD.encode(b"jpeg-bytes");
    let result = CallToolResult {
        content: vec![
            ContentBlock {
                kind: "image".into(),
                data: Some(png.clone()),
                mime_type: Some("image/png".into()),
                ..Default::default()
            },
            ContentBlock {
                kind: "resource".into(),
                resource: Some(ResourceContents {
                    uri: "mock://shot".into(),
                    mime_type: Some("image/jpeg".into()),
                    text: None,
                    blob: Some(jpeg.clone()),
                }),
                ..Default::default()
            },
        ],
        structured_content: None,
        is_error: None,
        meta: None,
    };
    let tool_result = convert_mcp_result("srv", "screenshot", result);
    assert_eq!(
        tool_result.images.len(),
        2,
        "both the image block and the image resource blob must reach the model"
    );
    assert_eq!(tool_result.images[0].data, png);
    assert_eq!(tool_result.images[0].mime_type, "image/png");
    assert_eq!(tool_result.images[1].data, jpeg);
    assert_eq!(tool_result.images[1].mime_type, "image/jpeg");
    assert!(
        tool_result.content.contains("[image: image/png]"),
        "the model-facing text keeps the block position: {}",
        tool_result.content
    );
}

/// A truncated MCP result advertises a readable spill file through
/// `details.fullOutputPath`, and that file holds the complete untruncated
/// output.
#[test]
fn mcp_truncated_tool_output_reports_a_readable_full_output_path() {
    let long = "x".repeat(50_000);
    let result = CallToolResult {
        content: vec![ContentBlock {
            kind: "text".into(),
            text: Some(long.clone()),
            ..Default::default()
        }],
        structured_content: None,
        is_error: None,
        meta: None,
    };
    let tool_result = convert_mcp_result("srv", "big", result);
    let path = tool_result
        .details
        .as_ref()
        .and_then(|details| details.get("fullOutputPath"))
        .and_then(Value::as_str)
        .expect("a truncated result must advertise fullOutputPath")
        .to_owned();
    assert!(
        tool_result
            .content
            .contains(&format!("[Output truncated. Full output: {path}]")),
        "the footer must name the readable file: {}",
        tool_result.content
    );
    assert!(
        tool_result.content.len() < 50_000,
        "the model-facing text stays capped"
    );
    let full = std::fs::read_to_string(&path).expect("the advertised spill file must be readable");
    assert_eq!(
        full, long,
        "the spill file holds the complete untruncated output"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn truncate_middle_caps_bytes_even_with_multibyte_characters() {
    // 4-byte characters: a character-counted cut would overshoot the byte
    // cap ~4x (upstream truncate.ts measures the cap in bytes).
    let long = "\u{1F600}".repeat(10_000); // 40_000 bytes of emoji
    let result = CallToolResult {
        content: vec![ContentBlock {
            kind: "text".into(),
            text: Some(long),
            ..Default::default()
        }],
        structured_content: None,
        is_error: None,
        meta: None,
    };
    let tool_result = convert_mcp_result("srv", "emoji", result);
    let cap = pir::mcp::manager::MCP_OUTPUT_MAX_BYTES;
    // The warning + cut markers ride on top of the kept head/tail; the whole
    // output stays close to the cap instead of `cap * 4`.
    assert!(
        tool_result.content.len() < cap + 1024,
        "byte cap respected: {} bytes for cap {cap}",
        tool_result.content.len()
    );
    // The kept content is valid UTF-8 (boundary fixup) and preserves both
    // ends.
    let content = &tool_result.content;
    assert!(content.contains("\u{1F600}"), "head characters survive");
    assert!(content.contains("truncated"));
}

// ---------------------------------------------------------------------------
// Declared output schema (DRIFT D25 / upstream `createMcpResultSchema`)
//
// These tests pin the Rust-built schema; its rendering (the upstream
// `renderToolOutputType` expectation — `structuredContent?: unknown` without a
// server schema, the relocated `$ref`s inside the declaration) is locked
// through the real codemode shim in `tests/codemode_declarations.rs`.
// ---------------------------------------------------------------------------

/// Build a bridge for a discovered tool with a given `outputSchema`.
fn bridge_with_output_schema(
    server: &str,
    tool: &str,
    output_schema: Option<Value>,
) -> McpToolBridge {
    let manager = McpManager::without_log(std::path::Path::new("."));
    let discovered = DiscoveredTool {
        server: server.to_owned(),
        tool: McpTool {
            name: tool.to_owned(),
            title: None,
            description: Some("A test tool".to_owned()),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema,
            annotations: None,
        },
        exposure: McpExposure::Codemode,
    };
    let registered = format!("mcp__{server}__{tool}");
    McpToolBridge::new(manager, discovered, registered)
}

/// Upstream `test/mcp-extension.test.ts` (`24382bedd`): every MCP tool declares
/// the `CallToolResult` scripts receive, so a codemode declaration spells out
/// `content`/`structuredContent`/`isError` instead of an undefined
/// `CallToolResult<T>`.
#[test]
fn mcp_tool_declares_the_call_tool_result_scripts_receive() {
    use pir::agent::tool_execution::AgentTool;

    // Without a server output schema: `structuredContent` is the empty schema
    // (upstream `67996ab75`, DRIFT D6 leftover), which the renderer turns into
    // `unknown` — never `{ type: "object" }`.
    let bridge = bridge_with_output_schema("sample", "search", None);
    let schema = bridge.output_schema().expect("declared output schema");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["content"]));
    assert_eq!(schema["additionalProperties"], true);
    assert_eq!(schema["properties"]["structuredContent"], json!({}));
    assert_eq!(
        schema["properties"]["content"]["items"]["properties"]["type"]["enum"],
        json!(["text", "image", "audio", "resource_link", "resource"])
    );
    assert_eq!(
        schema["properties"]["content"]["items"]["required"],
        json!(["type"])
    );
    assert_eq!(
        schema["properties"]["content"]["items"]["additionalProperties"],
        true
    );
    assert_eq!(schema["properties"]["isError"], json!({"type": "boolean"}));
    // `_meta` is dropped by `convert_mcp_result`, so it is not declared.
    assert!(schema["properties"].get("_meta").is_none());
}

/// The server's own output schema rides `structuredContent` with its local
/// `$ref`s relocated so they resolve as before, and data that looks like a
/// reference is left alone (upstream `relocateSchema`).
#[test]
fn mcp_tool_output_schema_relocates_server_references() {
    let server_schema = json!({
        "type": "object",
        "properties": {
            "hits": { "type": "array", "items": { "$ref": "#/$defs/Hit~1v1" } },
            "again": { "$ref": "#/properties/hits" },
            "children": { "type": "array", "items": { "$ref": "#" } },
            "default": { "$ref": "#/definitions/Hit" },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "required": ["hits"],
        "$defs": { "Hit/v1": {
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "default": { "$ref": "#/stays" },
        } },
        "definitions": { "Hit": {
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
        } },
    });
    let bridge = bridge_with_output_schema("sample", "search", Some(server_schema));
    use pir::agent::tool_execution::AgentTool;
    let schema = bridge.output_schema().expect("declared output schema");
    let structured = &schema["properties"]["structuredContent"];
    // Every local reference points below `structuredContent` now.
    assert_eq!(
        structured["properties"]["hits"]["items"]["$ref"],
        "#/properties/structuredContent/$defs/Hit~1v1"
    );
    assert_eq!(
        structured["properties"]["again"]["$ref"],
        "#/properties/structuredContent/properties/hits"
    );
    assert_eq!(
        structured["properties"]["children"]["items"]["$ref"],
        "#/properties/structuredContent"
    );
    assert_eq!(
        structured["properties"]["default"]["$ref"],
        "#/properties/structuredContent/definitions/Hit"
    );
    // A remote reference stays as sent.
    assert_eq!(
        structured["properties"]["remote"]["$ref"],
        "https://example.com/schema.json"
    );
    // Data that looks like a reference is kept as is.
    assert_eq!(
        structured["$defs"]["Hit/v1"]["default"],
        json!({ "$ref": "#/stays" })
    );
}

/// `relocate_schema` is the shared helper (public for the codemode declaration
/// tests too): pointers, `#`, map keywords, data keywords, arrays, and
/// non-string `$ref`s.
#[test]
fn relocate_schema_rewrites_only_local_references() {
    use pir::plugins::codemode::relocate_schema;
    let pointer = "#/properties/structuredContent";
    assert_eq!(
        relocate_schema(&json!({"$ref": "#"}), pointer),
        json!({"$ref": pointer})
    );
    assert_eq!(
        relocate_schema(&json!({"$ref": "#/$defs/A"}), pointer),
        json!({"$ref": "#/properties/structuredContent/$defs/A"})
    );
    assert_eq!(
        relocate_schema(&json!({"$ref": "https://x/y.json"}), pointer),
        json!({"$ref": "https://x/y.json"})
    );
    assert_eq!(
        relocate_schema(&json!({"$ref": 7}), pointer),
        json!({"$ref": 7})
    );
    assert_eq!(
        relocate_schema(&json!({"default": {"$ref": "#/stays"}}), pointer),
        json!({"default": {"$ref": "#/stays"}})
    );
    assert_eq!(
        relocate_schema(
            &json!({"properties": {"a": {"$ref": "#/$defs/A"}}}),
            pointer
        ),
        json!({"properties": {"a": {"$ref": "#/properties/structuredContent/$defs/A"}}})
    );
    assert_eq!(
        relocate_schema(&json!([{"$ref": "#"}, "x", null]), pointer),
        json!([{"$ref": pointer}, "x", null])
    );
}

// ---------------------------------------------------------------------------
// CLI typed errors
// ---------------------------------------------------------------------------

#[test]
fn mcp_login_required_error_points_at_the_session_command() {
    // The runtime sign-in error (upstream `McpOAuthAuthorizationRequiredError`
    // plus `signInRequiredMessage`, runtime.ts:77-80) names the server and the
    // in-session command that completes the sign-in (`/mcp`; the CLI's
    // `pir mcp login` hint is a `pir mcp add`/`check` message, not this one).
    let message = McpError::LoginRequired {
        server: "weather".into(),
    }
    .to_string();
    assert!(message.contains("requires sign-in"), "got: {message}");
    assert!(
        message.contains("Run /mcp to sign in."),
        "points at the in-session sign-in command: {message}"
    );
    assert!(
        !message.contains("pir mcp login"),
        "the agent-facing error must not point at the CLI: {message}"
    );
}

#[tokio::test]
async fn mcp_login_and_logout_require_configured_servers() {
    // `pir mcp login`/`logout` resolve the server from mcp.json first; an
    // unknown name fails before any OAuth work (upstream `cli.ts:227-233`).
    for command in [
        pir::coding_agent::cli::McpCommand::Login {
            server: "pir-test-not-configured".into(),
            timeout_seconds: 300,
        },
        pir::coding_agent::cli::McpCommand::Logout {
            server: "pir-test-not-configured".into(),
        },
    ] {
        let error = pir::coding_agent::cli::run_mcp_command(command)
            .await
            .expect_err("login/logout for an unconfigured server must fail");
        let message = error.to_string();
        assert!(
            message.contains("pir-test-not-configured"),
            "names the server, got: {message}"
        );
    }
}

// ---------------------------------------------------------------------------
// The built-in `sd` MCP server's replace engine (`pir mcp-sd`)
// ---------------------------------------------------------------------------

/// A request with sd's defaults (`dry_run: true`, everything else off), for
/// tests to override field by field.
fn sd_request(pattern: &str, replacement: &str) -> SdRequest {
    SdRequest {
        pattern: pattern.to_owned(),
        replacement: replacement.to_owned(),
        paths: Vec::new(),
        literal: false,
        case_insensitive: false,
        whole_word: false,
        multiline: false,
        dry_run: true,
        max_matches: None,
        max_files: None,
    }
}

/// A temp directory removed on drop: the engine's file tests need real paths
/// (it walks and rewrites the filesystem by design).
struct SdTempDir(PathBuf);

impl SdTempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-sd-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        // The engine expects a canonical root (what `pir mcp-sd --root` hands
        // it); containment is decided on canonical paths.
        let canonical = std::fs::canonicalize(&path).expect("canonical root");
        Self(canonical)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, contents).expect("write file");
        path
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.0.join(relative)).expect("read file")
    }
}

impl Drop for SdTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn sd_capture_references_follow_sd_semantics() {
    // `$1`/`$2` positional captures, `${name}` named captures, `$$` for a
    // literal `$`, and an unmatched group expanding to nothing.
    let mut request = sd_request(r"(\w+)-(\w+)", "$2:$1");
    assert_eq!(
        replace_text("alpha-beta gamma-delta", &request).expect("replace"),
        "beta:alpha delta:gamma"
    );

    request = sd_request(r"(?<key>\w+)=(\w+)", "${key} -> $2");
    assert_eq!(
        replace_text("mode=fast", &request).expect("replace"),
        "mode -> fast"
    );

    request = sd_request("price", "$$9");
    assert_eq!(replace_text("price", &request).expect("replace"), "$9");

    request = sd_request(r"(a)|(b)", "[$1$2]");
    assert_eq!(replace_text("a b", &request).expect("replace"), "[a] [b]");
}

#[test]
fn sd_literal_flag_disables_regex_syntax() {
    let mut request = sd_request("a.c", "X");
    assert_eq!(
        replace_text("abc a.c", &request).expect("replace"),
        "X X",
        "regex by default"
    );

    request.literal = true;
    assert_eq!(
        replace_text("abc a.c", &request).expect("replace"),
        "abc X",
        "literal: only the exact text"
    );

    // The whole pattern is escaped, metacharacters included.
    let mut request = sd_request("1+1 (sure)", "two");
    request.literal = true;
    assert_eq!(
        replace_text("1+1 (sure) and 1+1 sure", &request).expect("replace"),
        "two and 1+1 sure"
    );
}

#[test]
fn sd_case_insensitive_and_whole_word_flags() {
    let mut request = sd_request("cat", "dog");
    assert_eq!(
        replace_text("cat Cat CAT", &request).expect("replace"),
        "dog Cat CAT"
    );
    request.case_insensitive = true;
    assert_eq!(
        replace_text("cat Cat CAT", &request).expect("replace"),
        "dog dog dog"
    );

    let mut request = sd_request("cat", "dog");
    request.whole_word = true;
    assert_eq!(
        replace_text("cat concatenate cat.", &request).expect("replace"),
        "dog concatenate dog."
    );
}

#[test]
fn sd_multiline_flag_matches_across_lines() {
    // Default mode is line by line: a pattern containing a newline can never
    // match, and `.` stops at the line end.
    let request = sd_request(r"one\ntwo", "joined");
    assert_eq!(
        replace_text("one\ntwo\n", &request).expect("replace"),
        "one\ntwo\n"
    );
    let request = sd_request("one.*two", "joined");
    assert_eq!(
        replace_text("one\ntwo\n", &request).expect("replace"),
        "one\ntwo\n"
    );

    // Multiline mode: `.` matches the newline and the pattern spans lines.
    let mut request = sd_request(r"one\ntwo", "joined");
    request.multiline = true;
    assert_eq!(
        replace_text("one\ntwo\n", &request).expect("replace"),
        "joined\n"
    );
    let mut request = sd_request("one.*two", "joined");
    request.multiline = true;
    assert_eq!(
        replace_text("one\ntwo\n", &request).expect("replace"),
        "joined\n"
    );
}

#[test]
fn sd_line_mode_keeps_crlf_line_endings() {
    // The `\r` stays out of the matched line, so `$` anchors where a user
    // expects, and the CRLF pair survives the rewrite.
    let request = sd_request("^foo$", "FOO");
    assert_eq!(
        replace_text("foo\r\nfoo\nbar\r\n", &request).expect("replace"),
        "FOO\r\nFOO\nbar\r\n"
    );
}

#[test]
fn sd_dry_run_previews_and_leaves_files_untouched() {
    let dir = SdTempDir::new("dry-run");
    dir.write("notes.txt", "alpha beta\n");
    let request = sd_request("alpha", "gamma");

    let summary = sd_run(dir.path(), &request).expect("engine run");
    assert!(summary.dry_run);
    assert_eq!(summary.files_scanned, 1);
    assert_eq!(
        summary.files_changed, 1,
        "a dry run reports what WOULD change"
    );
    assert_eq!(summary.replacements, 1);
    assert!(!summary.truncated);
    assert_eq!(summary.binary_skipped, 0);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].path, "notes.txt");
    assert_eq!(summary.files[0].replacements, 1);
    assert!(
        summary.files[0]
            .preview
            .contains("line 1: `alpha` -> `gamma`"),
        "preview: {}",
        summary.files[0].preview
    );

    // The structured shape the MCP result carries.
    let structured = summary.to_json();
    assert_eq!(structured["dry_run"], json!(true));
    assert_eq!(structured["files_scanned"], json!(1));
    assert_eq!(structured["files_changed"], json!(1));
    assert_eq!(structured["replacements"], json!(1));
    assert_eq!(structured["truncated"], json!(false));
    assert_eq!(structured["binary_skipped"], json!(0));
    assert_eq!(structured["errors"], json!([]));
    assert_eq!(structured["files"][0]["path"], json!("notes.txt"));
    let (text, clipped) = summary.to_text();
    assert!(!clipped);
    assert!(text.contains("dry run"), "text: {text}");
    assert!(text.contains("would change"), "text: {text}");

    // Nothing was written.
    assert_eq!(dir.read("notes.txt"), "alpha beta\n");
}

#[test]
fn sd_real_run_rewrites_matched_files_and_reports_counts() {
    let dir = SdTempDir::new("write");
    dir.write("a.txt", "foo foo\nplain\n");
    dir.write("b.txt", "nothing to see\n");
    let mut request = sd_request("foo", "baz");
    request.dry_run = false;

    let summary = sd_run(dir.path(), &request).expect("engine run");
    assert!(!summary.dry_run);
    assert_eq!(summary.files_scanned, 2);
    assert_eq!(
        summary.files_changed, 1,
        "the no-match file is not a change"
    );
    assert_eq!(summary.replacements, 2);
    assert_eq!(summary.files[0].path, "a.txt");
    assert_eq!(summary.files[0].replacements, 2);
    assert_eq!(dir.read("a.txt"), "baz baz\nplain\n");
    assert_eq!(dir.read("b.txt"), "nothing to see\n");
    let (text, _) = summary.to_text();
    assert!(text.contains("1 file(s) changed"), "text: {text}");
}

#[test]
fn sd_skips_binary_and_non_utf8_files_and_never_rewrites_them() {
    let dir = SdTempDir::new("binary");
    // A NUL byte in the sniff window marks the file binary...
    dir.write("blob.bin", "foo\0foo");
    // ...and invalid UTF-8 is refused even without a NUL byte.
    std::fs::write(
        dir.path().join("latin1.bin"),
        [0xFF, 0xFE, b'f', b'o', b'o'],
    )
    .expect("write");
    dir.write("text.txt", "foo\n");
    let mut request = sd_request("foo", "bar");
    request.dry_run = false;

    let summary = sd_run(dir.path(), &request).expect("engine run");
    assert_eq!(summary.binary_skipped, 2);
    assert_eq!(summary.files_changed, 1);
    assert_eq!(summary.files[0].path, "text.txt");
    assert_eq!(dir.read("text.txt"), "bar\n");
    let (text, _) = summary.to_text();
    assert!(text.contains("skipped 2 binary"), "text: {text}");

    // The skipped bytes are untouched, byte for byte.
    assert_eq!(dir.read("blob.bin"), "foo\0foo");
    assert_eq!(
        std::fs::read(dir.path().join("latin1.bin")).expect("read"),
        [0xFF, 0xFE, b'f', b'o', b'o']
    );
}

#[test]
fn sd_directory_walks_respect_gitignore() {
    let dir = SdTempDir::new("gitignore");
    dir.write(".gitignore", "ignored.txt\nskipped/\n");
    dir.write("ignored.txt", "foo\n");
    dir.write("skipped/deep.txt", "foo\n");
    dir.write("covered.txt", "foo\n");
    let mut request = sd_request("foo", "bar");
    request.dry_run = false;

    let summary = sd_run(dir.path(), &request).expect("engine run");
    assert_eq!(summary.files_changed, 1, "files: {:?}", summary.files);
    assert_eq!(summary.files[0].path, "covered.txt");
    assert_eq!(dir.read("ignored.txt"), "foo\n");
    assert_eq!(dir.read("skipped/deep.txt"), "foo\n");
    assert_eq!(dir.read("covered.txt"), "bar\n");
}

#[test]
fn sd_explicitly_named_paths_override_the_walk_filters() {
    // Naming a file means touching it, gitignore or not (sd semantics); the
    // directory walk stays filtered. Every entry is resolved inside root.
    let dir = SdTempDir::new("paths");
    dir.write(".gitignore", "ignored.txt\n");
    dir.write("ignored.txt", "foo\n");
    dir.write("sub/one.txt", "foo\n");
    let mut request = sd_request("foo", "bar");
    request.dry_run = false;
    request.paths = vec!["ignored.txt".to_owned(), "sub".to_owned()];

    let summary = sd_run(dir.path(), &request).expect("engine run");
    let mut paths: Vec<&str> = summary
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["ignored.txt", "sub/one.txt"]);
    assert_eq!(dir.read("ignored.txt"), "bar\n");
    assert_eq!(dir.read("sub/one.txt"), "bar\n");
}

#[test]
fn sd_refuses_paths_outside_the_root() {
    let root = SdTempDir::new("root");
    let outside = SdTempDir::new("outside");
    let secret = outside.write("secret.txt", "foo\n");
    root.write("inside.txt", "foo\n");
    let mut request = sd_request("foo", "bar");
    request.dry_run = false;

    // An absolute path outside the root.
    request.paths = vec![secret.to_string_lossy().into_owned()];
    let error = sd_run(root.path(), &request).expect_err("outside path must be refused");
    assert!(error.contains("outside --root"), "got: {error}");

    // A relative path that climbs out with `..`.
    let outside_name = outside
        .path()
        .file_name()
        .expect("temp dir name")
        .to_string_lossy()
        .into_owned();
    request.paths = vec![format!("../{outside_name}/secret.txt")];
    let error = sd_run(root.path(), &request).expect_err("escaping path must be refused");
    assert!(error.contains("outside --root"), "got: {error}");

    // A path that does not exist is a clear error too, not a silent skip.
    request.paths = vec!["nope.txt".to_owned()];
    let error = sd_run(root.path(), &request).expect_err("missing path must be refused");
    assert!(error.contains("nope.txt"), "got: {error}");

    assert_eq!(outside.read("secret.txt"), "foo\n");
}

#[test]
fn sd_max_matches_and_max_files_bound_the_call_and_report_truncation() {
    let dir = SdTempDir::new("limits");
    dir.write("many.txt", "x x x\n");
    dir.write("other.txt", "x\n");

    // max_matches: only the first replacement is applied, the file keeps the
    // rest, and the summary says it is partial. The single named path keeps the
    // assertion independent of walk order.
    let mut request = sd_request("x", "y");
    request.dry_run = false;
    request.max_matches = Some(1);
    request.paths = vec!["many.txt".to_owned()];
    let summary = sd_run(dir.path(), &request).expect("engine run");
    assert_eq!(summary.replacements, 1);
    assert!(summary.truncated, "a hit match cap must be reported");
    assert_eq!(dir.read("many.txt"), "y x x\n");
    assert_eq!(dir.read("other.txt"), "x\n", "an unnamed file is untouched");
    let (text, _) = summary.to_text();
    assert!(text.contains("truncated"), "text: {text}");

    // max_files: the walk stops after one file and says so.
    let mut request = sd_request("x", "y");
    request.max_files = Some(1);
    let summary = sd_run(dir.path(), &request).expect("engine run");
    assert_eq!(summary.files_scanned, 1);
    assert!(summary.truncated);
}

#[test]
fn sd_arguments_validate_required_fields_and_unknown_keys() {
    let parsed = SdRequest::from_arguments(
        json!({"pattern": "a", "replacement": "b"})
            .as_object()
            .expect("object"),
    )
    .expect("minimal valid call");
    assert!(parsed.dry_run, "dry_run defaults to true");
    assert!(!parsed.literal && !parsed.case_insensitive && !parsed.whole_word);
    assert!(parsed.paths.is_empty());
    assert_eq!(parsed.max_matches, None);

    let cases = [
        json!({"replacement": "b"}),
        json!({"pattern": "a"}),
        json!({"pattern": "", "replacement": "b"}),
        json!({"pattern": "a", "replacement": "b", "dryrun": false}),
        json!({"pattern": "a", "replacement": "b", "dry_run": "no"}),
        json!({"pattern": "a", "replacement": "b", "paths": "x"}),
        json!({"pattern": "a", "replacement": "b", "paths": [""]}),
        json!({"pattern": "a", "replacement": "b", "max_matches": 0}),
        json!({"pattern": "a", "replacement": "b", "max_files": -1}),
    ];
    for case in cases {
        let arguments = case.as_object().expect("object");
        assert!(
            SdRequest::from_arguments(arguments).is_err(),
            "must be rejected: {case}"
        );
    }
    let unknown = SdRequest::from_arguments(
        json!({"pattern": "a", "replacement": "b", "dryrun": false})
            .as_object()
            .expect("object"),
    )
    .expect_err("unknown keys are rejected");
    assert!(unknown.contains("unknown argument"), "got: {unknown}");
}

#[test]
fn sd_tool_declaration_carries_the_documented_schema() {
    let tool = SdServer::tool();
    assert_eq!(tool.name, "sd");
    let schema = &*tool.input_schema;
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["required"], json!(["pattern", "replacement"]));
    for key in [
        "pattern",
        "replacement",
        "paths",
        "literal",
        "case_insensitive",
        "whole_word",
        "multiline",
        "dry_run",
        "max_matches",
        "max_files",
    ] {
        assert!(
            schema["properties"].get(key).is_some(),
            "input schema omits {key}"
        );
    }
    assert_eq!(schema["properties"]["dry_run"]["default"], json!(true));
    assert_eq!(schema["properties"]["paths"]["type"], json!("array"));
    assert!(tool.output_schema.is_some(), "the result shape is declared");
}
