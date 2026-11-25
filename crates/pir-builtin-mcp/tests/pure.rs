use pir_builtin_mcp::*;
use pir_plugin_api::BuiltinPlugin;

#[test]
fn tool_names_use_the_double_underscore_namespace() {
    // `mcpNamespace` rewrites `-` only and preserves case; the plain tool name
    // sanitizes every other character (upstream `createMcpToolName`).
    assert_eq!(mcp_namespace("my-server"), "mcp__my_server");
    assert_eq!(mcp_namespace("App"), "mcp__App");
    assert_eq!(mcp_tool_name("App", "read"), "mcp__App__read");
    assert_eq!(mcp_tool_name("my server", "x y"), "mcp__my_server__x_y");
    assert_eq!(plain_tool_name("a-b", "c.d"), "mcp__a_b__c_d");
    assert!(is_mcp_tool_name("mcp__a__b"));
    assert!(!is_mcp_tool_name("read"));
    assert_eq!(split_mcp_tool_name("mcp__App__read"), Some(("App", "read")));
    assert_eq!(split_mcp_tool_name("read"), None);
    assert_eq!(split_mcp_tool_name("mcp__bare"), None);
}

#[test]
fn exposure_resolution_prefers_override_then_selection() {
    assert_eq!(
        resolve_tool_exposure(McpExposure::Deferred, None, false),
        McpExposure::Deferred
    );
    assert_eq!(
        resolve_tool_exposure(McpExposure::Deferred, Some(McpExposure::Hidden), false),
        McpExposure::Hidden
    );
    assert_eq!(
        resolve_tool_exposure(McpExposure::Hidden, Some(McpExposure::Hidden), true),
        McpExposure::Inline
    );
    assert_eq!(McpExposure::parse("deferred"), McpExposure::Deferred);
    assert_eq!(McpExposure::parse("hidden"), McpExposure::Hidden);
    assert_eq!(McpExposure::parse("bogus"), McpExposure::Inline);
    assert_eq!(McpExposure::Deferred.as_str(), "deferred");
}

#[test]
fn plugin_identity() {
    assert_eq!(McpPlugin::new().info().path, "builtin:mcp");
}
