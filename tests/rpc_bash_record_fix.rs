//! Regression coverage for two confirmed RPC `bash` recording bugs:
//!
//! 1. `excludeFromContext` (`!!`-style) on the LOCAL execution path: the RPC
//!    handler parsed the flag but the local execution recorded with a
//!    hardcoded `false`, leaking excluded output into the LLM context. Fixed
//!    by threading the flag through `execute_bash_with_options`.
//! 2. Extension `user_bash` `Handled` results: the RPC handler returned the
//!    extension's result without recording it (the old comment claimed the
//!    session facade would, but nothing did), so the result never reached the
//!    transcript/context. Upstream records explicitly (rpc-mode.ts:569-573
//!    `recordBashResult`); the RPC host now does the same.
//!
//! In-process harness (`RpcServer::serve` over a tokio duplex) mirroring
//! `suites/rpc_extension_ui.rs`. `get_messages` exposes the session
//! projection, i.e. exactly the entries that would reach the LLM context
//! (`custom_message` records participate; excluded `custom` records do not).

use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fake provider plumbing (the model is never contacted)
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

type Responder = Arc<dyn Fn(usize) -> AssistantMessage + Send + Sync>;

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Temp dir + extension runtime + session factory
// ---------------------------------------------------------------------------

/// Per-test temp directory (session storage + extension file).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-rpc-bash-record-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write_extension(&self, label: &str, source: &str) -> std::path::PathBuf {
        let path = self.0.join(format!("{label}.ts"));
        let mut file = std::fs::File::create(&path).expect("create extension file");
        file.write_all(source.as_bytes()).expect("write extension");
        file.flush().expect("flush extension");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Start a REAL QuickJS extension runtime with one JS extension loaded.
async fn start_extension_runtime(temp: &TempDir, label: &str, source: &str) -> ExtensionHandle {
    let extension_path = temp.write_extension(label, source);
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "rpc".to_string(),
        has_ui: false,
        limits: Some(Default::default()),
        ..Default::default()
    })
    .expect("extension runtime should start");
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("extension load should not fail at the transport level");
    assert!(
        loaded.errors.is_empty(),
        "extension load errors: {:?}",
        loaded.errors
    );
    handle
}

/// Session factory; `handle` becomes the session's extension runtime.
fn session_factory(handle: Option<ExtensionHandle>) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: fake_registry(Arc::new(|_index| {
                assistant(vec![text("done")], StopReason::Stop)
            })),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: handle.clone(),
            tool_selection: None,
        })))
    })
}

// ---------------------------------------------------------------------------
// In-memory RPC client
// ---------------------------------------------------------------------------

struct RpcClient {
    writer: tokio::sync::mpsc::UnboundedSender<String>,
    reader: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl RpcClient {
    fn send(&self, value: serde_json::Value) {
        self.writer
            .send(serde_json::to_string(&value).expect("serialize"))
            .expect("send command");
    }

    /// Read protocol lines until `predicate` matches (30s per-line bound so a
    /// regression can never hang the suite).
    async fn wait_for(
        &mut self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..600 {
            let line = tokio::time::timeout(Duration::from_secs(30), self.reader.recv())
                .await
                .expect("read must not time out")
                .expect("protocol line");
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("rpc line must be valid JSON");
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected protocol line never arrived");
    }
}

async fn rpc_pair(label: &str, factory: SessionFactory) -> (RpcClient, TempDir) {
    let temp = TempDir::new(label);
    let server = RpcServer::new(temp.0.clone(), factory);
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client_stream);
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if client_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
        }
    });
    (
        RpcClient {
            writer: write_tx,
            reader: line_rx,
        },
        temp,
    )
}

async fn attach_session(client: &mut RpcClient) {
    client.send(serde_json::json!({ "type": "create" }));
    let created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    assert_eq!(created["success"], serde_json::json!(true), "{created}");
}

/// Send one `bash` command and return its (successful) response.
async fn send_bash(
    client: &mut RpcClient,
    id: &str,
    command: &str,
    exclude_from_context: bool,
) -> serde_json::Value {
    client.send(serde_json::json!({
        "id": id,
        "type": "bash",
        "command": command,
        "excludeFromContext": exclude_from_context,
    }));
    let response = client
        .wait_for(|value| value["command"] == "bash" && value["id"] == id)
        .await;
    assert_eq!(response["success"], serde_json::json!(true), "{response}");
    response
}

/// The LLM-context projection (`get_messages`) as one searchable blob.
async fn context_blob(client: &mut RpcClient) -> String {
    client.send(serde_json::json!({ "id": "gm", "type": "get_messages" }));
    let response = client
        .wait_for(|value| value["command"] == "get_messages")
        .await;
    assert_eq!(response["success"], serde_json::json!(true), "{response}");
    response["data"]["messages"].to_string()
}

// ---------------------------------------------------------------------------
// Bug 1: local execution honors `excludeFromContext`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_bash_exclude_from_context_controls_context_visibility() {
    let (mut client, _temp) = rpc_pair("local-exclude", session_factory(None)).await;
    attach_session(&mut client).await;

    // No flag: recorded into the LLM context.
    let response = send_bash(&mut client, "keep", "echo RPC_LOCAL_KEEP_OK", false).await;
    assert!(
        response["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("RPC_LOCAL_KEEP_OK"),
        "{response}"
    );
    // `excludeFromContext`: recorded, but OUT of the LLM context.
    let response = send_bash(&mut client, "excl", "echo RPC_LOCAL_EXCLUDED_OK", true).await;
    assert!(
        response["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("RPC_LOCAL_EXCLUDED_OK"),
        "{response}"
    );

    let blob = context_blob(&mut client).await;
    assert!(
        blob.contains("RPC_LOCAL_KEEP_OK"),
        "in-context bash record missing: {blob}"
    );
    assert!(
        !blob.contains("RPC_LOCAL_EXCLUDED_OK"),
        "excluded bash output leaked into the LLM context: {blob}"
    );
}

// ---------------------------------------------------------------------------
// Bug 2: extension `Handled` results are recorded (plus the Local fallthrough
// still honors the exclude flag)
// ---------------------------------------------------------------------------

const USER_BASH_EXTENSION: &str = r#"
export default function (pi) {
  pi.on("user_bash", (event) => {
    if (event.command === "ext-handled-keep") {
      return {
        result: { output: "EXT_KEEP_OUTPUT", exitCode: 0, cancelled: false, truncated: false },
      };
    }
    if (event.command === "ext-handled-excluded") {
      return {
        result: { output: "EXT_EXCLUDED_OUTPUT", exitCode: 0, cancelled: false, truncated: false },
      };
    }
    return undefined;
  });
}
"#;

#[tokio::test]
async fn extension_handled_bash_is_recorded_and_honors_the_exclude_flag() {
    let temp = TempDir::new("ext-record");
    let handle = start_extension_runtime(&temp, "user-bash", USER_BASH_EXTENSION).await;
    let (mut client, _temp) = rpc_pair("ext-record", session_factory(Some(handle))).await;
    attach_session(&mut client).await;

    // `Handled` (no flag): the extension's result is the response AND must be
    // recorded into the LLM context (upstream rpc-mode.ts:569-573).
    let response = send_bash(&mut client, "h-keep", "ext-handled-keep", false).await;
    assert_eq!(
        response["data"]["output"],
        serde_json::json!("EXT_KEEP_OUTPUT"),
        "{response}"
    );
    assert_eq!(
        response["data"]["isError"],
        serde_json::json!(false),
        "{response}"
    );

    // `Handled` + `excludeFromContext`: recorded, but out of the context.
    let response = send_bash(&mut client, "h-excl", "ext-handled-excluded", true).await;
    assert_eq!(
        response["data"]["output"],
        serde_json::json!("EXT_EXCLUDED_OUTPUT"),
        "{response}"
    );

    // The handler declines these commands: local execution fallthrough must
    // still honor the flag.
    let response = send_bash(&mut client, "l-keep", "echo RPC_EXT_LOCAL_KEEP_OK", false).await;
    assert!(
        response["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("RPC_EXT_LOCAL_KEEP_OK"),
        "{response}"
    );
    send_bash(
        &mut client,
        "l-excl",
        "echo RPC_EXT_LOCAL_EXCLUDED_OK",
        true,
    )
    .await;

    let blob = context_blob(&mut client).await;
    assert!(
        blob.contains("ext-handled-keep") && blob.contains("EXT_KEEP_OUTPUT"),
        "extension-handled bash result missing from the transcript/context: {blob}"
    );
    assert!(
        !blob.contains("ext-handled-excluded") && !blob.contains("EXT_EXCLUDED_OUTPUT"),
        "excluded extension-handled record leaked into the LLM context: {blob}"
    );
    assert!(
        blob.contains("RPC_EXT_LOCAL_KEEP_OK"),
        "local fallthrough record missing: {blob}"
    );
    assert!(
        !blob.contains("RPC_EXT_LOCAL_EXCLUDED_OK"),
        "excluded local fallthrough output leaked into the LLM context: {blob}"
    );
}
