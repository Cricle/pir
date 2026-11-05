//! Regression tests from the builtin-Python audit.
//!
//! Both defects were found through the real plugin registration path:
//!
//! - `json.dumps` float formatting diverged from CPython's `float.__repr__`
//!   in the negative-exponent range (`1e-05` became `0.00001`, `1e-07`
//!   became `1e-7`) because serde_json/ryu formats floats differently.
//! - `timeout_ms` was read with `Value::as_u64`, so a schema-valid JSON number
//!   such as `500.0` was ignored and the call silently used the 30s default.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::builtin::{PythonPlugin, PYTHON_TOOL_NAME};
use pir::plugins::plugin::{AgentPlugin, ToolBackend};
use pir::plugins::PluginHost;

fn host_tool(host: &PluginHost, name: &str) -> Arc<dyn AgentTool> {
    host.registry()
        .resolved_tools()
        .iter()
        .find(|entry| entry.0.name == name)
        .and_then(|entry| match &entry.1 {
            ToolBackend::Rust(tool) => Some(Arc::clone(tool)),
            ToolBackend::Js { .. } => None,
        })
        .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_dumps_float_text_matches_stdlib_through_plugin() {
    let plugin = Arc::new(PythonPlugin::with_config(true));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    let tool = host_tool(&host, PYTHON_TOOL_NAME);
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));

    let result = tool
        .run(
            &json!({ "code": r#"
import json

values = [
    1e20, 1e16, 1e15, 1e-4, 1e-5, 1e-7, 1.2345678901234567e-5,
    -1e-5, 5e-324, -0.0, 0.0, 1.0, 100.0, 0.1, 1234567890123456.0,
]
for value in values:
    fast = json.dumps([value])
    stdlib = json._pir_stdlib_dumps([value])
    assert fast == stdlib, (repr(value), fast, stdlib)

records = [{"score": i / 7.0, "tiny": i * 1e-8} for i in range(50)]
for kwargs in (dict(), dict(sort_keys=True), dict(indent=2), dict(ensure_ascii=False)):
    assert json.dumps(records, **kwargs) == json._pir_stdlib_dumps(records, **kwargs)
print("OK")
"# }),
            &ctx,
        )
        .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("OK"), "{}", result.content);
    plugin.on_shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fractional_timeout_ms_is_honored_and_resets_session() {
    let plugin = Arc::new(PythonPlugin::with_config(true));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    let tool = host_tool(&host, PYTHON_TOOL_NAME);
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));

    let warm = tool.run(&json!({ "code": "flag = 'kept'" }), &ctx).await;
    assert!(!warm.is_error, "{}", warm.content);

    let started = Instant::now();
    let result = tool
        .run(
            &json!({ "code": "while True:\n    pass", "timeout_ms": 500.0 }),
            &ctx,
        )
        .await;
    let elapsed = started.elapsed();

    assert!(result.is_error, "expected a timeout error");
    assert!(
        result.content.contains("timed out after 0.5s"),
        "fractional timeout_ms was ignored: {}",
        result.content
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the 500ms deadline took {elapsed:?}"
    );

    let after = tool
        .run(
            &json!({ "code": "'flag' in globals()", "timeout_ms": 5000 }),
            &ctx,
        )
        .await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(
        after.content, "False",
        "a timed-out call must reset the persistent session"
    );
    plugin.on_shutdown().await.expect("shutdown");
}
