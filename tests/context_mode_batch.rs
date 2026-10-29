//! Tests for `ctx_batch_execute`: multiple Python scripts in one call, each
//! auto-indexed under its own source label, with optional inline queries.
//!
//! Hermetic: the knowledge base lives in a per-test temp directory
//! (`ContextModePlugin::with_db_path`) and the scripts run on the embedded
//! RustPython interpreter (no system Python, no network).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::builtin::{
    ContextModePlugin, CTX_BATCH_EXECUTE_TOOL_NAME, CTX_SEARCH_TOOL_NAME, MAX_OUTPUT_CHARS,
};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// The embedded RustPython VM serializes user code behind a process-wide
/// execution lock (`src/plugins/builtin/python_vm.rs`), and cargo runs this
/// binary's tests concurrently. Without this gate a script can spend its whole
/// per-script deadline queued behind another test's script (the batch then
/// reports "timed out while queued" instead of the behavior under test). Hold
/// it for one whole tool call so every call's budget starts only once the
/// shared interpreter is free.
static PYTHON_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Self-cleaning temp directory.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-context-batch-{tag}-{}-{}",
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
    /// removal attempt. The underscore marks it as a lifetime guard, not data.
    _dir: TestDir,
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
            cwd,
            plugin,
            tools,
            _dir: dir,
        }
    }

    fn tool(&self, name: &str) -> Arc<dyn AgentTool> {
        self.tools
            .get(name)
            .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
            .clone()
    }

    async fn run(&self, name: &str, args: Value) -> ToolResult {
        // Every tool call may reach the shared interpreter; holding the gate
        // across the call keeps the per-script deadlines honest.
        let _serial = PYTHON_TEST_LOCK.lock().await;
        self.tool(name)
            .run(&args, &ToolContext::new(self.cwd.clone()))
            .await
    }

    async fn batch(&self, args: Value) -> ToolResult {
        self.run(CTX_BATCH_EXECUTE_TOOL_NAME, args).await
    }
}

fn parse_json(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("expected JSON result, got `{}` ({error})", result.content))
}

fn scripts(value: &Value) -> &Vec<Value> {
    value["scripts"]
        .as_array()
        .unwrap_or_else(|| panic!("`scripts` summary is an array: {value}"))
}

fn sections(value: &Value, query_index: usize) -> &Vec<Value> {
    value["queries"][query_index]["sections"]
        .as_array()
        .unwrap_or_else(|| panic!("query #{query_index} has sections: {value}"))
}

// ---------------------------------------------------------------------------
// Indexing + inline queries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_indexes_each_script_and_returns_sections_from_the_right_label() {
    let harness = Harness::new("index-query");
    let result = harness
        .batch(json!({
            "scripts": [
                { "label": "alpha", "code": "print('alpha_marker apples')" },
                { "label": "beta", "code": "print('beta_marker bananas')" }
            ],
            "queries": ["beta_marker"],
            "source": "demo"
        }))
        .await;
    assert!(!result.is_error, "{}", result.content);

    let value = parse_json(&result);
    assert_eq!(value["type"], "batch", "{value}");
    assert_eq!(value["source"], "demo", "{value}");
    let summaries = scripts(&value);
    assert_eq!(summaries.len(), 2, "{value}");
    assert_eq!(summaries[0]["label"], "alpha");
    assert_eq!(summaries[1]["label"], "beta");
    for summary in summaries {
        assert!(
            summary["indexedChunks"].as_u64().unwrap() >= 1,
            "each output is indexed: {value}"
        );
        assert_eq!(summary["isError"], false);
        assert!(summary.get("errorPreview").is_none());
    }

    assert_eq!(value["queries"][0]["query"], "beta_marker");
    let hits = sections(&value, 0);
    assert!(!hits.is_empty(), "the query matched the batch: {value}");
    for hit in hits {
        assert_eq!(
            hit["source"], "demo#beta",
            "the section comes from the right script: {value}"
        );
        assert!(hit["snippet"].is_string(), "{value}");
        assert!(hit["title"].is_string(), "{value}");
        // Search hits carry snippets and line hints, never the stored body.
        assert!(hit.get("content").is_none(), "{value}");
    }

    // Every script runs in a fresh namespace on the same lazy interpreter.
    assert_eq!(harness.plugin.python_spawn_count(), 1);
}

#[tokio::test]
async fn batch_sources_are_label_scoped_for_exact_source_filtering() {
    let harness = Harness::new("source-filter");
    let result = harness
        .batch(json!({
            "scripts": [
                { "label": "alpha", "code": "print('alpha_filter_marker')" },
                { "label": "beta", "code": "print('beta_filter_marker')" }
            ],
            "source": "filterbase"
        }))
        .await;
    assert!(!result.is_error, "{}", result.content);

    let alpha = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["alpha_filter_marker"], "source": "filterbase#alpha" }),
        )
        .await;
    assert!(!alpha.is_error, "{}", alpha.content);
    let alpha = parse_json(&alpha);
    let hits = alpha["queries"][0]["results"].as_array().unwrap();
    assert!(!hits.is_empty(), "the alpha label is searchable: {alpha}");
    assert!(hits.iter().all(|hit| hit["source"] == "filterbase#alpha"));

    // The base label is not a wildcard: the scripts live under `#label`.
    let base = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["alpha_filter_marker"], "source": "filterbase" }),
        )
        .await;
    assert!(!base.is_error, "{}", base.content);
    assert!(
        parse_json(&base)["queries"][0]["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the base label matches no chunk exactly"
    );

    let other = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["alpha_filter_marker"], "source": "filterbase#beta" }),
        )
        .await;
    assert!(
        parse_json(&other)["queries"][0]["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the wrong label does not match"
    );

    // A single-script batch indexes under the bare base source.
    let single = harness
        .batch(json!({
            "scripts": [{ "code": "print('solo_filter_marker')" }],
            "source": "solo"
        }))
        .await;
    assert!(!single.is_error, "{}", single.content);
    let solo = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["solo_filter_marker"], "source": "solo" }),
        )
        .await;
    let solo = parse_json(&solo);
    assert!(
        !solo["queries"][0]["results"].as_array().unwrap().is_empty(),
        "a one-script batch uses the base source: {solo}"
    );
}

#[tokio::test]
async fn default_source_is_stable_batch_hash() {
    let harness = Harness::new("default-source");
    let args = json!({
        "scripts": [{ "code": "print('default_source_marker')" }]
    });
    let first = harness.batch(args.clone()).await;
    assert!(!first.is_error, "{}", first.content);
    let first = parse_json(&first);
    let source = first["source"]
        .as_str()
        .unwrap_or_else(|| panic!("source label: {first}"));
    assert!(source.starts_with("batch:"), "{first}");
    let hint = first["hint"].as_str().expect("the no-query hint");
    assert_eq!(hint, format!("ctx_search(source: {source:?})"));

    // Re-running the same batch reuses the label (same cwd + first script).
    let second = harness.batch(args).await;
    let second = parse_json(&second);
    assert_eq!(second["source"], source);

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["default_source_marker"], "source": source }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    assert!(!parse_json(&search)["queries"][0]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// Failure isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failing_script_does_not_prevent_the_next_and_is_summarized() {
    let harness = Harness::new("fail-isolation");
    let result = harness
        .batch(json!({
            "scripts": [
                {
                    "label": "boom",
                    "code": "print('partial_marker')\n1 / 0"
                },
                { "label": "after", "code": "print('after_marker')" }
            ],
            "queries": ["after_marker"],
            "source": "failbatch"
        }))
        .await;
    assert!(
        !result.is_error,
        "a per-script failure does not fail the batch: {}",
        result.content
    );

    let value = parse_json(&result);
    let summaries = scripts(&value);
    assert_eq!(summaries[0]["isError"], true, "{value}");
    let preview = summaries[0]["errorPreview"]
        .as_str()
        .unwrap_or_else(|| panic!("failing script gets a preview: {value}"));
    assert!(
        preview.contains("ZeroDivisionError"),
        "the preview names the failure: {preview}"
    );
    assert!(
        !value
            .to_string()
            .contains("Traceback (most recent call last)"),
        "the traceback is summarized, not pasted: {value}"
    );
    assert!(
        !result.content.contains("partial_marker"),
        "partial stdout stays in the index: {}",
        result.content
    );
    assert!(
        summaries[0]["indexedChunks"].as_u64().unwrap() >= 1,
        "the partial stdout before the failure is still indexed: {value}"
    );
    assert_eq!(summaries[1]["isError"], false, "{value}");

    let hits = sections(&value, 0);
    assert!(
        !hits.is_empty(),
        "the script after the failure ran and was indexed: {value}"
    );
    assert!(hits.iter().all(|hit| hit["source"] == "failbatch#after"));
}

#[tokio::test]
async fn each_script_gets_a_fresh_namespace() {
    let harness = Harness::new("fresh-namespace");
    let result = harness
        .batch(json!({
            "scripts": [
                { "code": "leaked = 41" },
                { "code": "print(leaked)" }
            ],
            "source": "fresh"
        }))
        .await;
    assert!(!result.is_error, "{}", result.content);

    let value = parse_json(&result);
    let summaries = scripts(&value);
    assert_eq!(summaries[0]["label"], "script-1", "{value}");
    assert_eq!(summaries[1]["label"], "script-2", "{value}");
    assert_eq!(summaries[0]["bytes"], 0, "{value}");
    assert_eq!(summaries[0]["indexedChunks"], 0, "{value}");
    assert_eq!(summaries[1]["isError"], true, "{value}");
    assert!(
        summaries[1]["errorPreview"]
            .as_str()
            .unwrap_or_default()
            .contains("NameError"),
        "the second script starts from a fresh namespace: {value}"
    );
    assert_eq!(
        harness.plugin.python_spawn_count(),
        1,
        "all scripts share the one lazy interpreter"
    );
}

// ---------------------------------------------------------------------------
// Bounds + timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn script_and_query_bounds_are_enforced_before_execution() {
    let harness = Harness::new("bounds");

    let empty = harness.batch(json!({ "scripts": [] })).await;
    assert!(empty.is_error);
    assert!(
        empty.content.contains("at least 1"),
        "got: {}",
        empty.content
    );

    let too_many: Vec<Value> = (0..17)
        .map(|index| json!({ "code": format!("print({index})") }))
        .collect();
    let rejected = harness.batch(json!({ "scripts": too_many })).await;
    assert!(rejected.is_error);
    assert!(
        rejected.content.contains("at most 16"),
        "got: {}",
        rejected.content
    );

    let empty_code = harness
        .batch(json!({ "scripts": [{ "code": "   \n " }] }))
        .await;
    assert!(empty_code.is_error);
    assert!(
        empty_code.content.contains("empty `code`"),
        "got: {}",
        empty_code.content
    );

    let too_many_queries: Vec<String> = (0..33).map(|index| format!("query{index}")).collect();
    let rejected = harness
        .batch(json!({
            "scripts": [{ "code": "print('bounds')" }],
            "queries": too_many_queries
        }))
        .await;
    assert!(rejected.is_error);
    assert!(
        rejected.content.contains("at most 32 queries"),
        "got: {}",
        rejected.content
    );

    let oversized = "a".repeat(513);
    let rejected = harness
        .batch(json!({
            "scripts": [{ "code": "print('bounds')" }],
            "queries": [oversized]
        }))
        .await;
    assert!(rejected.is_error);
    assert!(
        rejected.content.contains("limited to 512 bytes"),
        "got: {}",
        rejected.content
    );

    assert!(
        !harness.plugin.has_python_worker(),
        "rejected arguments never start the interpreter"
    );
}

#[tokio::test]
async fn timeout_applies_per_script_and_the_batch_continues() {
    let harness = Harness::new("timeout");
    let started = Instant::now();
    let result = harness
        .batch(json!({
            "scripts": [
                { "label": "runaway", "code": "while True:\n    pass" },
                { "label": "next", "code": "print('survived_marker')" }
            ],
            "queries": ["survived_marker"],
            "timeout": 2,
            "source": "timeoutbatch"
        }))
        .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "the 2s per-script deadline landed promptly: {elapsed:?}"
    );
    assert!(
        !result.is_error,
        "a timed-out script does not fail the batch: {}",
        result.content
    );

    let value = parse_json(&result);
    let summaries = scripts(&value);
    assert_eq!(summaries[0]["isError"], true, "{value}");
    assert!(
        summaries[0]["errorPreview"]
            .as_str()
            .unwrap_or_default()
            .contains("timed out"),
        "the timeout is summarized: {value}"
    );
    assert_eq!(summaries[1]["isError"], false, "{value}");
    let hits = sections(&value, 0);
    assert!(
        !hits.is_empty(),
        "the next script ran after the timeout: {value}"
    );
    assert!(hits.iter().all(|hit| hit["source"] == "timeoutbatch#next"));
}

// ---------------------------------------------------------------------------
// Output caps + compactness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_script_output_cap_is_respected_and_bodies_stay_out_of_context() {
    let harness = Harness::new("cap");
    let result = harness
        .batch(json!({
            "scripts": [{ "label": "big", "code": "print('x' * 1_100_000)" }]
        }))
        .await;
    assert!(!result.is_error, "{}", result.content);
    let value = parse_json(&result);
    let summary = &scripts(&value)[0];
    let bytes = summary["bytes"].as_u64().unwrap();
    assert!(
        bytes > MAX_OUTPUT_CHARS as u64,
        "the write-time cap itself was reached: {bytes}"
    );
    assert!(
        bytes <= MAX_OUTPUT_CHARS as u64 + 200,
        "never more than cap + truncation notice: {bytes}"
    );
    assert!(
        summary["indexedChunks"].as_u64().unwrap() >= 1,
        "the capped output is still indexed: {value}"
    );

    // The batch result is a summary: no raw output, so it stays tiny.
    assert!(
        result.content.len() < 5_000,
        "only the summary enters context, got {} chars",
        result.content.len()
    );
    assert!(
        !result.content.contains(&"x".repeat(1_000)),
        "the indexed body is never echoed back"
    );
}

#[tokio::test]
async fn query_sections_are_snippets_not_full_chunks() {
    let harness = Harness::new("snippet");
    let result = harness
        .batch(json!({
            "scripts": [
                { "code": "print('needle_body ' + 'Z' * 6000)" }
            ],
            "queries": ["needle_body"],
            "source": "bodybatch"
        }))
        .await;
    assert!(!result.is_error, "{}", result.content);

    let value = parse_json(&result);
    assert!(
        scripts(&value)[0]["bytes"].as_u64().unwrap() >= 6_000,
        "{value}"
    );
    let hits = sections(&value, 0);
    assert!(!hits.is_empty(), "the marker is findable: {value}");
    for hit in hits {
        let snippet = hit["snippet"].as_str().unwrap();
        assert!(
            snippet.contains("needle_body"),
            "the snippet still identifies the match: {value}"
        );
        assert!(
            !snippet.contains(&"Z".repeat(600)),
            "the snippet window is bounded, not the whole chunk"
        );
    }
    assert!(
        !result.content.contains(&"Z".repeat(600)),
        "the full indexed body never enters context"
    );
}
