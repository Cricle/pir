//! Hook parity tests for the native `context-mode` builtin.
//!
//! The plugin registers tools AND the upstream PreToolUse/PostToolUse hooks
//! natively (`AgentPlugin::events` / `on_event`):
//!
//! - `tool_call`: `curl`/`wget` (and PowerShell `iwr`/`Invoke-WebRequest`)
//!   commands, plus simple `cat`/`head`/`tail`/`less` and direct `read` calls
//!   on a single file larger than 64 KiB, are blocked with `ctx_*` guidance;
//!   `PIR_CONTEXT_MODE_ROUTING=0` disables that gate;
//! - `tool_result`: a result text larger than 20 KiB is auto-indexed under
//!   `session:<id>` and is retrievable through `ctx_search`;
//!   `PIR_CONTEXT_MODE_INDEX_RESULTS=0` disables it;
//! - `session_start`: identity capture only (no DB/Python work);
//! - `ctx_doctor`: local diagnostics, read-only and lazy.
//!
//! All tests are hermetic (per-test temp directory + explicit DB path).
//! Environment mutations are guarded by a process-wide async lock because
//! the switches are process globals.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::extensions::SessionContextSnapshot;
use pir::plugins::builtin::{
    ContextModePlugin, CTX_DOCTOR_TOOL_NAME, CTX_INDEX_TOOL_NAME, CTX_SEARCH_TOOL_NAME,
};
use pir::plugins::plugin::{
    AgentPlugin, PluginEvent, PluginEventOutcome, PluginRegistrations, RegistrationContext,
    ToolBackend,
};
use pir::plugins::PluginHost;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// Serializes every test in this file: the hook switches are environment
/// variables and therefore process-global.
async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-context-mode-hooks-{tag}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap_or_else(|error| panic!("temp dir: {error}"));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    cwd: PathBuf,
    plugin: Arc<ContextModePlugin>,
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    /// Last so the tools (and their SQLite handle) drop before the directory
    /// removal attempt.
    _dir: TestDir,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = TestDir::new(tag);
        let cwd = dir.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap_or_else(|error| panic!("cwd: {error}"));
        let plugin = Arc::new(ContextModePlugin::with_db_path(
            dir.path().join("context-mode.db"),
        ));
        let mut registrations = PluginRegistrations::default();
        plugin
            .register(&mut RegistrationContext::new(&mut registrations))
            .unwrap_or_else(|error| panic!("register: {error}"));
        let tools = registrations
            .tools
            .into_iter()
            .filter_map(|(definition, backend)| match backend {
                ToolBackend::Rust(tool) => Some((definition.name, tool)),
                ToolBackend::Js { .. } => None,
            })
            .collect();
        // Mirror the session host's push (session id + cwd) so relative
        // paths and the session source label resolve without a real session.
        plugin.on_session_context(&SessionContextSnapshot {
            session_id: Some("base-session".to_owned()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..SessionContextSnapshot::default()
        });
        Self {
            _dir: dir,
            cwd,
            plugin,
            tools,
        }
    }

    fn tool(&self, name: &str) -> Arc<dyn AgentTool> {
        self.tools
            .get(name)
            .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
            .clone()
    }

    async fn run(&self, name: &str, args: Value) -> ToolResult {
        self.tool(name)
            .run(&args, &ToolContext::new(self.cwd.clone()))
            .await
    }

    async fn emit(&self, event: &str, payload: &mut Value) -> PluginEventOutcome {
        self.plugin.on_event(PluginEvent::new(event, payload)).await
    }
}

fn parse_json(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("expected JSON result, got `{}` ({error})", result.content))
}

fn tool_call(tool: &str, input: Value) -> Value {
    json!({
        "type": "tool_call",
        "toolCallId": "call-1",
        "toolName": tool,
        "input": input,
    })
}

fn assert_blocked(outcome: PluginEventOutcome) -> String {
    match outcome {
        PluginEventOutcome::ToolCall {
            block: true,
            reason: Some(reason),
            terminate: false,
        } => reason,
        other => panic!("expected a native block with a reason, got {other:?}"),
    }
}

fn assert_unblocked(outcome: PluginEventOutcome) {
    assert!(
        matches!(outcome, PluginEventOutcome::None),
        "expected no handler decision, got {outcome:?}"
    );
}

fn write_big_file(path: &Path, bytes: usize) {
    std::fs::write(path, vec![b'x'; bytes]).unwrap_or_else(|error| panic!("write: {error}"));
}

fn large_result_text(marker: &str) -> String {
    let mut text = String::new();
    while text.len() < 25 * 1024 {
        text.push_str("log line alpha beta gamma\n");
    }
    text.push_str(marker);
    text
}

fn tool_result_payload(tool: &str, input: Value, text: String) -> Value {
    json!({
        "type": "tool_result",
        "toolCallId": "call-1",
        "toolName": tool,
        "input": input,
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

async fn search_hits(harness: &Harness, query: &str) -> Vec<Value> {
    let search = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "query": query }))
        .await;
    if search.is_error && search.content.contains("knowledge base is empty") {
        return Vec::new();
    }
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    value["queries"][0]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("search results: {value}"))
        .clone()
}

// ---------------------------------------------------------------------------
// tool_call routing (PreToolUse)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routing_blocks_downloaders_with_ctx_guidance() {
    let _env = env_lock().await;
    let harness = Harness::new("downloaders");
    for (tool, command, expected) in [
        ("bash", "curl https://example.com/page", "curl"),
        (
            "shell_command",
            "wget -qO- https://example.com/page",
            "wget",
        ),
        (
            "powershell",
            "Invoke-WebRequest https://example.com/page",
            "Invoke-WebRequest",
        ),
        ("powershell", "iwr https://example.com/page", "iwr"),
        (
            "bash",
            "npm test && curl -s https://example.com/page",
            "curl",
        ),
    ] {
        let mut payload = tool_call(tool, json!({ "command": command }));
        let reason = assert_blocked(harness.emit("tool_call", &mut payload).await);
        assert!(
            reason.contains(expected),
            "reason names `{expected}`: {reason}"
        );
        assert!(
            reason.contains("ctx_fetch_and_index") && reason.contains("ctx_execute"),
            "reason points at the context-mode tools: {reason}"
        );
    }

    // PowerShell-only names are not blocked in a POSIX shell (a script named
    // `iwr` on Linux is unrelated guidance).
    let mut payload = tool_call("bash", json!({ "command": "iwr https://example.com" }));
    assert_unblocked(harness.emit("tool_call", &mut payload).await);
}

#[tokio::test]
async fn routing_blocks_large_simple_file_reads_only() {
    let _env = env_lock().await;
    let harness = Harness::new("file-reads");
    let big = harness.cwd.join("big.log");
    let small = harness.cwd.join("small.log");
    write_big_file(&big, 64 * 1024 + 1);
    std::fs::write(&small, b"small enough").unwrap();

    for command in [
        "cat big.log",
        "cat -n big.log",
        "head big.log",
        "head -n 5 big.log",
        "tail -n 20 big.log",
        "less big.log",
    ] {
        let mut payload = tool_call("bash", json!({ "command": command }));
        let reason = assert_blocked(harness.emit("tool_call", &mut payload).await);
        assert!(
            reason.contains("ctx_execute_file"),
            "`{command}` is routed to ctx_execute_file: {reason}"
        );
        assert!(
            reason.contains("65537") && reason.contains("65536"),
            "reason reports the file size and the limit: {reason}"
        );
    }

    let mut payload = tool_call(
        "read",
        json!({ "path": big.to_string_lossy(), "offset": 0, "limit": 10 }),
    );
    let reason = assert_blocked(harness.emit("tool_call", &mut payload).await);
    assert!(
        reason.contains("ctx_execute_file") && reason.contains("65537"),
        "a direct large read is routed too: {reason}"
    );

    // Small files, non-read commands, complex pipelines and the ctx tools
    // themselves stay untouched.
    for (tool, input) in [
        ("bash", json!({ "command": "cat small.log" })),
        ("bash", json!({ "command": "head -n 5 small.log" })),
        ("bash", json!({ "command": "cat big.log | grep x" })),
        ("bash", json!({ "command": "cat big.log small.log" })),
        ("bash", json!({ "command": "ls -la" })),
        ("read", json!({ "path": small.to_string_lossy() })),
        ("read", json!({ "path": "missing.log" })),
        (
            "ctx_execute",
            json!({ "code": "import os\nprint(os.getcwd())" }),
        ),
        ("ctx_search", json!({ "queries": ["anything"] })),
    ] {
        let mut payload = tool_call(tool, input);
        assert_unblocked(harness.emit("tool_call", &mut payload).await);
    }
}

#[tokio::test]
async fn routing_env_knob_disables_blocking() {
    let _env = env_lock().await;
    let harness = Harness::new("routing-off");
    write_big_file(&harness.cwd.join("big.log"), 64 * 1024 + 1);

    let _guard = EnvGuard::set("PIR_CONTEXT_MODE_ROUTING", "0");
    for (tool, input) in [
        (
            "bash",
            json!({ "command": "curl https://example.com/page" }),
        ),
        ("bash", json!({ "command": "cat big.log" })),
        ("read", json!({ "path": "big.log" })),
    ] {
        let mut payload = tool_call(tool, input);
        assert_unblocked(harness.emit("tool_call", &mut payload).await);
    }
}

#[tokio::test]
async fn host_dispatch_reaches_the_native_hook() {
    let _env = env_lock().await;
    let harness = Harness::new("host");
    let host = PluginHost::build(None, vec![harness.plugin.clone() as Arc<dyn AgentPlugin>]);
    assert!(
        host.has_native_handlers("tool_call")
            && host.has_native_handlers("tool_result")
            && host.has_native_handlers("session_start"),
        "events() declares the hook surface"
    );

    let mut payload = tool_call("bash", json!({ "command": "curl -s https://example.com" }));
    let outcome = host.emit("tool_call", &mut payload).await.unwrap();
    assert_eq!(outcome.result["block"], json!(true), "{outcome:?}");
    assert!(
        outcome.result["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("ctx_fetch_and_index")),
        "{outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// tool_result auto-index (PostToolUse)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn large_tool_result_is_indexed_under_the_session_source() {
    let _env = env_lock().await;
    let harness = Harness::new("auto-index");

    let mut start = json!({
        "type": "session_start",
        "reason": "startup",
        "sessionId": "sess-42",
    });
    assert_unblocked(harness.emit("session_start", &mut start).await);

    let mut payload = tool_result_payload(
        "bash",
        json!({ "command": "npm test" }),
        large_result_text("unique_marker_zebra"),
    );
    let before = payload.clone();
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert_eq!(
        payload, before,
        "auto-indexing must never mutate the tool result the model sees"
    );

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "unique_marker_zebra", "source": "session:sess-42" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    let results = value["queries"][0]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("search results: {value}"));
    assert!(!results.is_empty(), "the large result was indexed: {value}");
    assert_eq!(results[0]["source"], json!("session:sess-42"));
    assert_eq!(
        results[0]["title"],
        json!("bash: npm"),
        "the title is compact (tool + first command token): {value}"
    );

    let doctor = harness.run(CTX_DOCTOR_TOOL_NAME, json!({})).await;
    let report = parse_json(&doctor);
    assert_eq!(report["counters"]["indexedResults"], json!(1), "{report}");
    assert_eq!(report["database"]["sources"], json!(1), "{report}");
}

#[tokio::test]
async fn tool_result_source_falls_back_to_the_tool_name() {
    let _env = env_lock().await;
    // No session_start and no session context push: the source label falls
    // back to `tool:<name>`.
    let dir = TestDir::new("fallback-source");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let plugin = Arc::new(ContextModePlugin::with_db_path(
        dir.path().join("context-mode.db"),
    ));
    let mut registrations = PluginRegistrations::default();
    plugin
        .register(&mut RegistrationContext::new(&mut registrations))
        .unwrap();
    let tools: BTreeMap<String, Arc<dyn AgentTool>> = registrations
        .tools
        .into_iter()
        .filter_map(|(definition, backend)| match backend {
            ToolBackend::Rust(tool) => Some((definition.name, tool)),
            ToolBackend::Js { .. } => None,
        })
        .collect();

    let mut payload = tool_result_payload(
        "read",
        json!({ "path": "/tmp/report.txt" }),
        large_result_text("fallback_marker"),
    );
    assert_unblocked(
        plugin
            .on_event(PluginEvent::new("tool_result", &mut payload))
            .await,
    );

    let search = tools[CTX_SEARCH_TOOL_NAME]
        .run(
            &json!({ "query": "fallback_marker" }),
            &ToolContext::new(cwd.clone()),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    let results = value["queries"][0]["results"].as_array().unwrap();
    assert_eq!(results[0]["source"], json!("tool:read"), "{value}");
}

#[tokio::test]
async fn small_tool_result_is_not_indexed() {
    let _env = env_lock().await;
    let harness = Harness::new("small-result");
    let mut payload = tool_result_payload(
        "bash",
        json!({ "command": "echo hi" }),
        "tiny_marker under the twenty kibibyte threshold".to_owned(),
    );
    assert_unblocked(harness.emit("tool_result", &mut payload).await);

    let search = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "query": "tiny_marker" }))
        .await;
    assert!(
        search.is_error && search.content.contains("knowledge base is empty"),
        "nothing was indexed: {}",
        search.content
    );
}

#[tokio::test]
async fn index_results_env_knob_suppresses_auto_indexing() {
    let _env = env_lock().await;
    let harness = Harness::new("index-off");
    let _guard = EnvGuard::set("PIR_CONTEXT_MODE_INDEX_RESULTS", "0");

    let mut payload = tool_result_payload(
        "bash",
        json!({ "command": "npm test" }),
        large_result_text("suppressed_marker"),
    );
    assert_unblocked(harness.emit("tool_result", &mut payload).await);

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "suppressed_marker" }),
        )
        .await;
    assert!(
        search.is_error && search.content.contains("knowledge base is empty"),
        "the env switch suppressed indexing: {}",
        search.content
    );
}

#[tokio::test]
async fn tool_result_threshold_boundary_is_preserved() {
    let _env = env_lock().await;
    let harness = Harness::new("threshold-boundary");

    // The gate is `text.len() <= 20 * 1024` bytes, counted on the joined text.
    const THRESHOLD: usize = 20 * 1024;

    // Exactly at the threshold: not indexed.
    let mut at = "a".repeat(THRESHOLD - 1 - "boundary_at".len());
    at.push_str(" boundary_at");
    assert_eq!(at.len(), THRESHOLD);
    let mut payload = tool_result_payload("bash", json!({ "command": "echo at" }), at);
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert!(search_hits(&harness, "boundary_at").await.is_empty());

    // An empty first block contributes no separator (the join only inserts
    // '\n' before a block when the accumulated text is non-empty), so a
    // second block of exactly THRESHOLD bytes stays at the threshold and must
    // not be indexed.
    let mut second = "g".repeat(THRESHOLD - 1 - "empty_first_marker".len());
    second.push_str(" empty_first_marker");
    assert_eq!(second.len(), THRESHOLD);
    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "call-empty-first",
        "toolName": "bash",
        "input": { "command": "echo empty-first" },
        "content": [
            { "type": "text", "text": "" },
            { "type": "text", "text": second },
        ],
        "isError": false,
    });
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert!(search_hits(&harness, "empty_first_marker").await.is_empty());

    // The same shape one byte over the threshold must be indexed.
    let mut second = "h".repeat(THRESHOLD + 1 - 1 - "empty_first_over".len());
    second.push_str(" empty_first_over");
    assert_eq!(second.len(), THRESHOLD + 1);
    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "call-empty-first-over",
        "toolName": "bash",
        "input": { "command": "echo empty-first-over" },
        "content": [
            { "type": "text", "text": "" },
            { "type": "text", "text": second },
        ],
        "isError": false,
    });
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert!(!search_hits(&harness, "empty_first_over").await.is_empty());

    // One byte over: indexed.
    let mut over = "b".repeat(THRESHOLD + 1 - 1 - "boundary_over".len());
    over.push_str(" boundary_over");
    assert_eq!(over.len(), THRESHOLD + 1);
    let mut payload = tool_result_payload("bash", json!({ "command": "echo over" }), over);
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert!(!search_hits(&harness, "boundary_over").await.is_empty());

    // Two blocks: the '\n' separator counts toward the gate. 10 KiB + 1 + 10 KiB
    // is 20481 bytes, so this must be indexed (a length probe that forgot the
    // separator would wrongly skip it).
    let first = format!("{} first_block_marker", "c".repeat(10 * 1024 - 19));
    let second = format!("{} second_block_marker", "d".repeat(10 * 1024 - 20));
    assert_eq!(first.len() + 1 + second.len(), THRESHOLD + 1);
    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "call-multi-over",
        "toolName": "bash",
        "input": { "command": "echo multi" },
        "content": [
            { "type": "text", "text": first },
            { "type": "text", "text": second },
        ],
        "isError": false,
    });
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert!(!search_hits(&harness, "second_block_marker")
        .await
        .is_empty());

    // Two blocks whose joined length (including the separator) is exactly the
    // threshold: still not indexed.
    let first = format!("{} third_block_marker", "e".repeat(10 * 1024 - 20));
    let second = format!("{} fourth_block", "f".repeat(10 * 1024 - 13));
    assert_eq!(first.len() + 1 + second.len(), THRESHOLD);
    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "call-multi-at",
        "toolName": "bash",
        "input": { "command": "echo multi-at" },
        "content": [
            { "type": "text", "text": first },
            { "type": "text", "text": second },
        ],
        "isError": false,
    });
    assert_unblocked(harness.emit("tool_result", &mut payload).await);
    assert!(search_hits(&harness, "third_block_marker").await.is_empty());
}

#[tokio::test]
async fn routing_tokenizer_equivalence_edges() {
    let _env = env_lock().await;
    let harness = Harness::new("route-edges");
    let big = harness.cwd.join("big.txt");
    write_big_file(&big, 70 * 1024);

    // Leading env assignments and wrappers still resolve the real executable.
    for command in [
        "A=1 B=2 C=3 D=4 E=5 curl https://example.com/data",
        "sudo curl https://example.com/data",
        "cd /tmp && curl https://example.com/data",
        "A=1 B=2 C=3 D=4 E=5 cat big.txt",
        "command cat big.txt",
    ] {
        let mut payload = tool_call("bash", json!({ "command": command }));
        let reason = assert_blocked(harness.emit("tool_call", &mut payload).await);
        assert!(
            reason.contains("context-mode routing"),
            "unexpected reason for `{command}`: {reason}"
        );
    }

    // A routing-looking word in an argument is not an executable: no block.
    for command in [
        "git commit -m concatenate",
        "rg --files-with-matches head",
        "cargo test -- --nocapture",
    ] {
        let mut payload = tool_call("bash", json!({ "command": command }));
        assert_unblocked(harness.emit("tool_call", &mut payload).await);
    }

    // Small files and read-tool calls below the size gate remain untouched.
    let small = harness.cwd.join("small.txt");
    std::fs::write(&small, b"tiny").unwrap();
    for payload in [
        tool_call("bash", json!({ "command": "cat small.txt" })),
        tool_call("read", json!({ "path": small.to_string_lossy() })),
    ] {
        let mut payload = payload;
        assert_unblocked(harness.emit("tool_call", &mut payload).await);
    }
}

// ---------------------------------------------------------------------------
// ctx_doctor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn doctor_reports_lazy_state_then_database_counts() {
    let _env = env_lock().await;
    let harness = Harness::new("doctor");

    let lazy = harness.run(CTX_DOCTOR_TOOL_NAME, json!({})).await;
    assert!(!lazy.is_error, "{}", lazy.content);
    let value = parse_json(&lazy);
    assert_eq!(value["python"]["status"], json!("not started"), "{value}");
    assert_eq!(value["python"]["workerStarted"], json!(false), "{value}");
    assert_eq!(value["python"]["spawnCount"], json!(0), "{value}");
    assert_eq!(value["database"]["exists"], json!(false), "{value}");
    assert_eq!(value["database"]["open"], json!(false), "{value}");
    assert_eq!(value["database"]["chunks"], json!(0), "{value}");
    assert_eq!(value["database"]["schemaVersion"], json!(0), "{value}");
    assert_eq!(
        value["database"]["fts5Available"],
        json!(true),
        "the bundled SQLite has FTS5: {value}"
    );
    assert!(
        value["database"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("context-mode.db")),
        "{value}"
    );
    assert_eq!(value["hooks"]["routingEnabled"], json!(true), "{value}");
    assert_eq!(
        value["hooks"]["indexResultsEnabled"],
        json!(true),
        "{value}"
    );
    assert_eq!(
        value["hooks"]["fileRoutingThresholdBytes"],
        json!(64 * 1024),
        "{value}"
    );
    assert_eq!(
        value["hooks"]["toolResultIndexThresholdBytes"],
        json!(20 * 1024),
        "{value}"
    );
    assert!(
        lazy.structured_content.is_some(),
        "ctx_doctor carries structuredContent"
    );

    let indexed = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "content": "# Alpha\n\nalpha body", "source": "doc" }),
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);

    let report = harness.run(CTX_DOCTOR_TOOL_NAME, json!({})).await;
    assert!(!report.is_error, "{}", report.content);
    let value = parse_json(&report);
    assert_eq!(value["database"]["exists"], json!(true), "{value}");
    assert_eq!(value["database"]["open"], json!(true), "{value}");
    assert_eq!(value["database"]["schemaVersion"], json!(1), "{value}");
    assert_eq!(value["database"]["fts5Available"], json!(true), "{value}");
    assert_eq!(value["database"]["chunks"], json!(1), "{value}");
    assert_eq!(value["database"]["sources"], json!(1), "{value}");
    assert!(
        value["database"]["sizeBytes"].as_u64().unwrap_or(0) > 0,
        "{value}"
    );
    assert_eq!(
        value["python"]["status"],
        json!("not started"),
        "doctor itself must not spawn the interpreter: {value}"
    );
}
