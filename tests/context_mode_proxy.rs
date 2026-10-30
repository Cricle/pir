//! Regression: `ctx_fetch_and_index` must use the configured egress proxy.
//!
//! Reported bug: "context-mode has no network". The fetch client used a bare
//! `reqwest::Client`, and reqwest is built with `default-features = false`
//! (its `system-proxy` feature is off), so every OS/env proxy was silently
//! ignored. Provider traffic (which resolves `httpProxy` explicitly) worked,
//! while `ctx_fetch_and_index` went direct and failed in proxied deployments.
//!
//! Local-only reproduction: a dead `.invalid` host can only be reached
//! through the proxy; the proxy answers itself, so a green result proves the
//! request was proxied. No external network is touched.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::builtin::{ContextModePlugin, CTX_FETCH_AND_INDEX_TOOL_NAME};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Harness {
    cwd: PathBuf,
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    dir: PathBuf,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-ctx-proxy-{tag}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let plugin = ContextModePlugin::with_db_path(dir.join("context-mode.db"));
        let mut registrations = PluginRegistrations::default();
        plugin
            .register(&mut RegistrationContext::new(&mut registrations))
            .unwrap();
        let tools = registrations
            .tools
            .into_iter()
            .filter_map(|(definition, backend)| match backend {
                ToolBackend::Rust(tool) => Some((definition.name, tool)),
                ToolBackend::Js { .. } => None,
            })
            .collect();
        Self { cwd, tools, dir }
    }

    fn tool(&self, name: &str) -> Arc<dyn AgentTool> {
        self.tools.get(name).expect("tool registered").clone()
    }

    async fn run(&self, name: &str, args: serde_json::Value) -> ToolResult {
        self.tool(name)
            .run(&args, &ToolContext::new(self.cwd.clone()))
            .await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A minimal HTTP proxy: records the absolute-form request line and answers
/// `proxied` itself. Keeps running until the process exits.
fn spawn_proxy(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let first_line = request.lines().next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(first_line);
            let body = "proxied";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

#[tokio::test]
async fn fetch_goes_through_the_configured_proxy() {
    // Serialize env mutation in this binary (one test today; the guard keeps
    // future siblings honest).
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _env_guard = ENV_LOCK.lock().await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_proxy(Arc::clone(&seen));

    let saved_proxy = std::env::var("PI_HTTP_PROXY").ok();
    let saved_no_proxy = std::env::var("NO_PROXY").ok();
    let saved_no_proxy_lc = std::env::var("no_proxy").ok();
    // The settings scope is empty in this test process, so PI_HTTP_PROXY is
    // the resolved proxy. A dead `.invalid` host proves the proxy was used:
    // direct DNS resolution cannot succeed.
    std::env::set_var("PI_HTTP_PROXY", format!("http://127.0.0.1:{port}"));
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let harness = Harness::new("proxy");
    let result = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({
                "url": "http://ctx-proxy-check.invalid/page",
                "force": true
            }),
        )
        .await;

    // Restore before assertions so a failure cannot leak the env var.
    match saved_proxy {
        Some(value) => std::env::set_var("PI_HTTP_PROXY", value),
        None => std::env::remove_var("PI_HTTP_PROXY"),
    }
    match saved_no_proxy {
        Some(value) => std::env::set_var("NO_PROXY", value),
        None => std::env::remove_var("NO_PROXY"),
    }
    match saved_no_proxy_lc {
        Some(value) => std::env::set_var("no_proxy", value),
        None => std::env::remove_var("no_proxy"),
    }

    assert!(!result.is_error, "fetch failed: {}", result.content);
    let value: serde_json::Value = serde_json::from_str(&result.content).expect("metadata JSON");
    assert_eq!(value["status"], json!(200), "metadata: {value}");
    assert_eq!(value["bytes"], json!("proxied".len()));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the proxy must receive exactly one hop");
    assert!(
        seen[0].contains("ctx-proxy-check.invalid"),
        "the proxy saw an absolute-form request: {:?}",
        seen[0]
    );
}
