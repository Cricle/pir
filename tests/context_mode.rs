//! Tests for the native `context-mode` builtin: tool registration/schemas,
//! the "think in code" execution contract (only stdout enters context, fresh
//! namespace per call), confinement of `ctx_execute_file`, the output cap and
//! the intent-driven auto-index path.
//!
//! All tests are hermetic: the knowledge base lives in a per-test temp
//! directory (`ContextModePlugin::with_db_path`) and the Python worker is the
//! embedded RustPython interpreter (no system Python, no network).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::extensions::SessionContextSnapshot;
use pir::plugins::builtin::{
    ContextModePlugin, CTX_BATCH_EXECUTE_TOOL_NAME, CTX_DOCTOR_TOOL_NAME,
    CTX_EXECUTE_FILE_TOOL_NAME, CTX_EXECUTE_TOOL_NAME, CTX_FETCH_AND_INDEX_TOOL_NAME,
    CTX_INDEX_TOOL_NAME, CTX_PURGE_TOOL_NAME, CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME,
    MAX_OUTPUT_CHARS,
};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// Self-cleaning temp directory.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-context-mode-{tag}-{}-{}",
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
    plugin: ContextModePlugin,
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    /// Last so the tools (and their SQLite handle) drop before the directory
    /// removal attempt.
    dir: TestDir,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = TestDir::new(tag);
        let cwd = dir.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap_or_else(|error| panic!("cwd: {error}"));
        let plugin = ContextModePlugin::with_db_path(dir.path().join("context-mode.db"));
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
        Self {
            dir,
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

    fn dir(&self) -> &Path {
        self.dir.path()
    }
}

fn parse_json(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("expected JSON result, got `{}` ({error})", result.content))
}

// ---------------------------------------------------------------------------
// Registration / schemas
// ---------------------------------------------------------------------------

#[test]
fn registers_exactly_the_nine_context_mode_tools() {
    let harness = Harness::new("registration");
    let names: Vec<&str> = harness.tools.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec![
            CTX_BATCH_EXECUTE_TOOL_NAME,
            CTX_DOCTOR_TOOL_NAME,
            CTX_EXECUTE_TOOL_NAME,
            CTX_EXECUTE_FILE_TOOL_NAME,
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            CTX_INDEX_TOOL_NAME,
            CTX_PURGE_TOOL_NAME,
            CTX_SEARCH_TOOL_NAME,
            CTX_STATS_TOOL_NAME,
        ],
        "exactly the nine ctx_* tools are registered"
    );

    let execute = harness.tool(CTX_EXECUTE_TOOL_NAME);
    let parameters = execute.parameters().expect("ctx_execute schema");
    assert_eq!(parameters["required"], json!(["code"]));
    assert!(parameters["properties"]["code"].is_object());
    assert!(parameters["properties"]["timeout"].is_object());
    assert!(parameters["properties"]["intent"].is_object());
    assert!(
        execute.description().contains("Only stdout enters context"),
        "description states the core contract: {}",
        execute.description()
    );
    assert!(
        execute
            .description()
            .contains("write analysis code, not data dumps"),
        "description steers toward analysis code: {}",
        execute.description()
    );

    let execute_file = harness.tool(CTX_EXECUTE_FILE_TOOL_NAME);
    let parameters = execute_file.parameters().expect("ctx_execute_file schema");
    let required = parameters["required"].as_array().unwrap();
    assert!(required.contains(&json!("path")));
    assert!(required.contains(&json!("code")));
    assert!(parameters["properties"]["timeout"].is_object());
    assert!(parameters["properties"]["intent"].is_object());
    assert!(execute_file.description().contains("FILE_CONTENT"));
    assert!(
        execute_file
            .description()
            .contains("for analysis, not editing"),
        "description separates analysis from editing: {}",
        execute_file.description()
    );

    let batch = harness.tool(CTX_BATCH_EXECUTE_TOOL_NAME);
    let parameters = batch.parameters().expect("ctx_batch_execute schema");
    assert_eq!(parameters["required"], json!(["scripts"]));
    assert_eq!(parameters["properties"]["scripts"]["minItems"], json!(1));
    assert_eq!(parameters["properties"]["scripts"]["maxItems"], json!(16));
    assert!(parameters["properties"]["queries"].is_object());
    assert!(parameters["properties"]["source"].is_object());
    assert!(parameters["properties"]["timeout"].is_object());
    assert!(
        batch
            .description()
            .contains("MULTIPLE Python analyses in one call"),
        "description steers toward batching: {}",
        batch.description()
    );
    assert!(
        batch.description().contains("never echoed in full"),
        "description states the raw output stays in the sandbox: {}",
        batch.description()
    );
    assert!(
        batch.has_external_side_effects(),
        "batch execution writes the knowledge base"
    );

    let index = harness.tool(CTX_INDEX_TOOL_NAME);
    let parameters = index.parameters().expect("ctx_index schema");
    for field in ["content", "path", "source", "contentType"] {
        assert!(
            parameters["properties"][field].is_object(),
            "ctx_index has `{field}`"
        );
    }

    let search = harness.tool(CTX_SEARCH_TOOL_NAME);
    let parameters = search.parameters().expect("ctx_search schema");
    assert!(parameters["properties"]["queries"].is_object());
    assert!(parameters["properties"]["query"].is_object());
    assert!(parameters["properties"]["source"].is_object());
    assert!(parameters["properties"]["contentType"].is_object());
    assert!(parameters["properties"]["limit"].is_object());

    let stats = harness.tool(CTX_STATS_TOOL_NAME);
    let parameters = stats.parameters().expect("ctx_stats schema");
    assert_eq!(parameters["properties"], json!({}));

    let purge = harness.tool(CTX_PURGE_TOOL_NAME);
    let parameters = purge.parameters().expect("ctx_purge schema");
    assert!(parameters["properties"]["confirm"].is_object());
    assert!(parameters["properties"]["source"].is_object());

    let fetch = harness.tool(CTX_FETCH_AND_INDEX_TOOL_NAME);
    let parameters = fetch.parameters().expect("ctx_fetch_and_index schema");
    assert_eq!(parameters["required"], json!(["url"]));
    assert!(parameters["properties"]["source"].is_object());
    assert!(parameters["properties"]["force"].is_object());
    assert!(parameters["properties"]["ttl"].is_object());
}

#[test]
fn builtin_factory_list_carries_context_mode() {
    assert!(
        pir::plugins::builtin::builtin_names().contains(&"context-mode"),
        "builtin:context-mode is in the factory list"
    );
    assert!(
        pir::plugins::builtin::instantiate_builtin("context-mode").is_some(),
        "instantiate_builtin resolves the context-mode plugin"
    );
    let definition =
        pir::plugins::builtin::builtin_extension_def("context-mode").expect("definition");
    assert!(definition.default_enabled);
    assert!(definition.replaceable);
}

#[tokio::test]
async fn python_worker_and_index_are_lazy() {
    let harness = Harness::new("lazy");
    // Construction (and registration) must not spawn the VM.
    assert!(!harness.plugin.has_python_worker());
    assert_eq!(harness.plugin.python_spawn_count(), 0);

    // Pure knowledge-base calls use SQLite only.
    let indexed = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "content": "# Lazy\n\nlazy body marker", "source": "lazy" }),
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);
    assert!(
        !harness.plugin.has_python_worker(),
        "ctx_index must not start the interpreter"
    );
    let searched = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": ["marker"] }))
        .await;
    assert!(!searched.is_error, "{}", searched.content);
    assert!(
        !harness.plugin.has_python_worker(),
        "ctx_search must not start the interpreter"
    );
    assert_eq!(harness.plugin.python_spawn_count(), 0);

    // The first execute starts exactly one worker; later calls reuse it.
    let first = harness
        .run(CTX_EXECUTE_TOOL_NAME, json!({ "code": "print('one')" }))
        .await;
    assert!(!first.is_error, "{}", first.content);
    assert!(harness.plugin.has_python_worker());
    assert_eq!(harness.plugin.python_spawn_count(), 1);
    let second = harness
        .run(CTX_EXECUTE_TOOL_NAME, json!({ "code": "print('two')" }))
        .await;
    assert!(!second.is_error, "{}", second.content);
    assert_eq!(
        harness.plugin.python_spawn_count(),
        1,
        "the worker is reused across calls"
    );
}

// ---------------------------------------------------------------------------
// ctx_execute semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn execute_prints_only_findings_not_reprs() {
    let harness = Harness::new("findings");
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "values = [1, 2, 3, 4]\nprint(sum(values))\nvalues" }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content.trim(),
        "10",
        "only stdout enters context; bare expressions are not echoed"
    );
    assert!(
        !result.content.contains("[1, 2, 3, 4]"),
        "the data never enters the result: {}",
        result.content
    );
}

#[tokio::test]
async fn fresh_namespace_per_call_but_interpreter_reused() {
    let harness = Harness::new("fresh-scope");
    let first = harness
        .run(CTX_EXECUTE_TOOL_NAME, json!({ "code": "leaked = 41" }))
        .await;
    assert!(!first.is_error, "{}", first.content);
    assert_eq!(first.content, "(no output)");

    let second = harness
        .run(CTX_EXECUTE_TOOL_NAME, json!({ "code": "print(leaked)" }))
        .await;
    assert!(
        second.is_error,
        "a name from an earlier call must not exist: {}",
        second.content
    );
    assert!(
        second.content.contains("NameError"),
        "traceback present, got: {}",
        second.content
    );
    assert_eq!(
        harness.plugin.python_spawn_count(),
        1,
        "the namespace is fresh; the interpreter is reused"
    );
}

#[tokio::test]
async fn uncaught_exception_returns_traceback() {
    let harness = Harness::new("traceback");
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "print('before')\n1 / 0" }),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("ZeroDivisionError"),
        "got: {}",
        result.content
    );
    assert!(
        result.content.contains("before"),
        "partial stdout is kept, got: {}",
        result.content
    );
}

#[tokio::test]
async fn timeout_interrupts_runaway_loop() {
    let harness = Harness::new("timeout");
    let started = Instant::now();
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "while True:\n    pass", "timeout": 2 }),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(result.is_error, "expected a timeout error");
    assert!(
        result.content.contains("timed out"),
        "got: {}",
        result.content
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the interrupt must land well before the default deadline: {elapsed:?}"
    );
}

#[tokio::test]
async fn oversized_output_is_capped() {
    let harness = Harness::new("cap");
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "print('x' * 1_100_000)" }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.len() > MAX_OUTPUT_CHARS,
        "output was produced and capped, got {} chars",
        result.content.len()
    );
    assert!(
        result.content.len() < MAX_OUTPUT_CHARS + 200,
        "cap + notice, got {} chars",
        result.content.len()
    );
    assert!(result.content.contains("truncated"));
}

#[tokio::test]
async fn empty_code_is_rejected() {
    let harness = Harness::new("empty-code");
    let result = harness
        .run(CTX_EXECUTE_TOOL_NAME, json!({ "code": "   \n " }))
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("code"));
}

#[tokio::test]
async fn intent_auto_indexes_large_stdout_instead_of_dumping_it() {
    let harness = Harness::new("intent");
    let code = r#"
lines = []
for i in range(200):
    lines.append(f"entry {i:03d}: needle value {i} " + "x" * 24)
print("\n".join(lines))
"#;
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": code, "intent": "needle" }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Indexed"),
        "large output is indexed: {}",
        result.content
    );
    assert!(
        result.content.contains("ctx_search"),
        "the response points at ctx_search: {}",
        result.content
    );
    assert!(
        result.content.contains("needle"),
        "matching sections are described: {}",
        result.content
    );
    // The whole ~9 KiB dump would be >9000 chars; the section report is small.
    assert!(
        result.content.len() < 6_000,
        "only titles + snippets returned, got {} chars",
        result.content.len()
    );

    // The auto-indexed output is queryable afterwards.
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["needle"], "source": "execute:python" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    assert!(!value["queries"][0]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// ctx_execute_file: injection + confinement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn execute_file_injects_file_content_and_returns_only_prints() {
    let harness = Harness::new("file-inject");
    std::fs::write(harness.cwd.join("data.txt"), "TOP_SECRET_abc\n42\n")
        .unwrap_or_else(|error| panic!("write fixture: {error}"));

    let result = harness
        .run(
            CTX_EXECUTE_FILE_TOOL_NAME,
            json!({
                "path": "data.txt",
                "code": "print(FILE_CONTENT.count('\\n'))\nprint(FILE_PATH.endswith('data.txt'))\nprint(len(FILE_CONTENT))"
            }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        !result.content.contains("TOP_SECRET_abc"),
        "the raw file bytes never enter the result: {}",
        result.content
    );
    assert!(
        result.content.contains("True"),
        "FILE_PATH is the resolved absolute path: {}",
        result.content
    );
    // Two newlines, so three lines in the file.
    assert!(result.content.contains('2'), "got: {}", result.content);
}

#[tokio::test]
async fn execute_file_rejects_escapes_and_non_files() {
    let harness = Harness::new("file-escape");
    let outside = harness.dir().join("outside.txt");
    std::fs::write(&outside, "outside secret").unwrap_or_else(|error| panic!("fixture: {error}"));

    let absolute = harness
        .run(
            CTX_EXECUTE_FILE_TOOL_NAME,
            json!({ "path": outside.to_string_lossy(), "code": "print(len(FILE_CONTENT))" }),
        )
        .await;
    assert!(absolute.is_error, "absolute escape must be rejected");
    assert!(
        absolute.content.contains("escapes"),
        "got: {}",
        absolute.content
    );

    let relative = harness
        .run(
            CTX_EXECUTE_FILE_TOOL_NAME,
            json!({ "path": "../outside.txt", "code": "print(len(FILE_CONTENT))" }),
        )
        .await;
    assert!(relative.is_error, "`..` escape must be rejected");
    assert!(
        relative.content.contains("escapes"),
        "got: {}",
        relative.content
    );

    let directory = harness
        .run(
            CTX_EXECUTE_FILE_TOOL_NAME,
            json!({ "path": ".", "code": "print(1)" }),
        )
        .await;
    assert!(directory.is_error, "a directory is not a file");
    assert!(
        directory.content.contains("regular file") || directory.content.contains("not"),
        "got: {}",
        directory.content
    );

    let missing = harness
        .run(
            CTX_EXECUTE_FILE_TOOL_NAME,
            json!({ "path": "does-not-exist.txt", "code": "print(1)" }),
        )
        .await;
    assert!(missing.is_error, "a missing file must be rejected");
}

#[test]
fn execute_file_rejects_symlink_escape_when_supported() {
    // Symlinks need privileges on Windows; the rejection path is still
    // exercised wherever the platform allows creating one.
    let harness = Harness::new("file-symlink");
    let outside = harness.dir().join("link-target.txt");
    std::fs::write(&outside, "linked secret").unwrap_or_else(|error| panic!("fixture: {error}"));
    let link = harness.cwd.join("link.txt");
    let created = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(&outside, &link).is_ok()
        }
    };
    if !created {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("runtime: {error}"));
    let result = runtime.block_on(harness.run(
        CTX_EXECUTE_FILE_TOOL_NAME,
        json!({ "path": "link.txt", "code": "print(len(FILE_CONTENT))" }),
    ));
    assert!(result.is_error, "symlink escape must be rejected");
    assert!(
        result.content.contains("escapes"),
        "got: {}",
        result.content
    );
}

#[tokio::test]
async fn execute_file_rejects_files_over_32_mib() {
    let harness = Harness::new("file-large");
    let path = harness.cwd.join("huge.bin");
    let file = std::fs::File::create(&path).unwrap_or_else(|error| panic!("fixture: {error}"));
    file.set_len(32 * 1024 * 1024 + 1)
        .unwrap_or_else(|error| panic!("sparse file: {error}"));
    let result = harness
        .run(
            CTX_EXECUTE_FILE_TOOL_NAME,
            json!({ "path": "huge.bin", "code": "print(len(FILE_CONTENT))" }),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("32 MiB"),
        "the 32 MiB guard is named: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// The default DB path and its live connection follow `(cwd, base)`
// (env-audit D10).
// ---------------------------------------------------------------------------

/// A host-pushed cwd change must move the *open connection*, not just the
/// memoized path: `ctx_index` in project A, then `ctx_stats` in project B must
/// report B's database (and share nothing with A). This drives the real
/// `IndexStore::call` reopen seam, which resolves the path after taking the
/// connection lock so a push racing a call cannot strand the handle.
#[tokio::test]
async fn default_database_path_and_connection_follow_a_changed_session_cwd() {
    let dir = TestDir::new("db-follow");
    let base = dir.path().join("agent-base");
    let cwd_a = dir.path().join("project-a");
    let cwd_b = dir.path().join("project-b");
    for cwd in [&cwd_a, &cwd_b] {
        std::fs::create_dir_all(cwd).unwrap_or_else(|error| panic!("cwd: {error}"));
    }

    let plugin = ContextModePlugin::with_db_base_dir(&base);
    // Register the tools against the same plugin, so index/stats share one
    // store (the Harness owns its plugin, so build the registry by hand).
    let mut registrations = PluginRegistrations::default();
    plugin
        .register(&mut RegistrationContext::new(&mut registrations))
        .unwrap_or_else(|error| panic!("register: {error}"));
    let tools: BTreeMap<String, Arc<dyn AgentTool>> = registrations
        .tools
        .into_iter()
        .filter_map(|(definition, backend)| match backend {
            ToolBackend::Rust(tool) => Some((definition.name, tool)),
            ToolBackend::Js { .. } => None,
        })
        .collect();
    let run = |name: &str, args: Value, cwd: &Path| {
        let tool = tools
            .get(name)
            .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
            .clone();
        let cwd = cwd.to_path_buf();
        async move { tool.run(&args, &ToolContext::new(cwd)).await }
    };
    let push_cwd = |plugin: &ContextModePlugin, cwd: &Path| {
        plugin.on_session_context(&SessionContextSnapshot {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..SessionContextSnapshot::default()
        });
    };

    // Project A: index one marker and open the connection.
    push_cwd(&plugin, &cwd_a);
    let indexed = run(
        CTX_INDEX_TOOL_NAME,
        json!({ "content": "# A\n\nalpha-marker", "source": "notes-a" }),
        &cwd_a,
    )
    .await;
    assert!(!indexed.is_error, "{}", indexed.content);
    let stats_a = run(CTX_STATS_TOOL_NAME, json!({}), &cwd_a).await;
    assert!(!stats_a.is_error, "{}", stats_a.content);
    let stats_a = parse_json(&stats_a);
    let path_a = stats_a["dbPath"]
        .as_str()
        .unwrap_or_else(|| panic!("dbPath: {stats_a}"))
        .to_owned();
    assert!(path_a.starts_with(&base.join("context-mode").to_string_lossy() as &str));
    assert_eq!(stats_a["chunks"], json!(1), "project A holds its own chunk");

    // Project B, same store: the live connection moved and B starts empty.
    push_cwd(&plugin, &cwd_b);
    let stats_b = run(CTX_STATS_TOOL_NAME, json!({}), &cwd_b).await;
    assert!(!stats_b.is_error, "{}", stats_b.content);
    let stats_b = parse_json(&stats_b);
    let path_b = stats_b["dbPath"]
        .as_str()
        .unwrap_or_else(|| panic!("dbPath: {stats_b}"))
        .to_owned();
    assert_ne!(path_b, path_a, "a changed cwd must move the open database");
    assert_eq!(
        stats_b["chunks"],
        json!(0),
        "project B must not read project A's index"
    );

    // Back to A: the earlier database (and its chunk) is reached again.
    push_cwd(&plugin, &cwd_a);
    let stats_a_again = run(CTX_STATS_TOOL_NAME, json!({}), &cwd_a).await;
    assert!(!stats_a_again.is_error, "{}", stats_a_again.content);
    let stats_a_again = parse_json(&stats_a_again);
    assert_eq!(stats_a_again["dbPath"], json!(path_a));
    assert_eq!(stats_a_again["chunks"], json!(1));
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_requires_exactly_one_of_content_and_path() {
    let harness = Harness::new("index-validation");
    let neither = harness.run(CTX_INDEX_TOOL_NAME, json!({})).await;
    assert!(neither.is_error);
    assert!(
        neither.content.contains("exactly one"),
        "got: {}",
        neither.content
    );
    let both = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "content": "# A", "path": "README.md" }),
        )
        .await;
    assert!(both.is_error);
    assert!(both.content.contains("exactly one"));
}
