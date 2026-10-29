//! Tests for the `context-mode` knowledge base (SQLite/FTS5): chunking,
//! BM25 search ordering, multi-query dedupe, filters, directory-walk bounds,
//! purge, stats, and `ctx_fetch_and_index` against a local HTTP server
//! (including the TTL cache).
//!
//! Hermetic: every test owns a temp DB via
//! `ContextModePlugin::with_db_path`; the fetch test serves its own page from
//! a loopback TCP listener and never touches the network.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::builtin::{
    ContextModePlugin, CTX_EXECUTE_TOOL_NAME, CTX_FETCH_AND_INDEX_TOOL_NAME, CTX_INDEX_TOOL_NAME,
    CTX_PURGE_TOOL_NAME, CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME,
};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-context-index-{tag}-{}-{}",
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
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    /// Keeps the temp directory alive until the tools (and their SQLite
    /// handle) have dropped.
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
            _dir: dir,
            cwd,
            tools,
        }
    }

    async fn run(&self, name: &str, args: Value) -> ToolResult {
        let tool = self
            .tools
            .get(name)
            .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
            .clone();
        tool.run(&args, &ToolContext::new(self.cwd.clone())).await
    }

    async fn index(&self, content: &str, source: &str, content_type: Option<&str>) -> Value {
        let mut args = json!({ "content": content, "source": source });
        if let Some(content_type) = content_type {
            args["contentType"] = json!(content_type);
        }
        let result = self.run(CTX_INDEX_TOOL_NAME, args).await;
        assert!(!result.is_error, "ctx_index failed: {}", result.content);
        parse_json(&result)
    }
}

fn parse_json(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("expected JSON result, got `{}` ({error})", result.content))
}

fn results_of(value: &Value, query_index: usize) -> Vec<Value> {
    value["queries"][query_index]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("missing results array in {value}"))
        .clone()
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn markdown_heading_chunking_keeps_fences_intact() {
    let harness = Harness::new("chunk-fences");
    let content = "# Alpha\n\nalpha intro\n\n```python\n# not a heading\nprint('fencecode')\n```\n\nstill alpha\n\n# Beta\n\nbeta text\n";
    let indexed = harness.index(content, "fences", None).await;
    assert_eq!(
        indexed["indexed"]["chunks"], 2,
        "one chunk per heading; the fenced `#` line must not split: {indexed}"
    );

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["fencecode"], "source": "fences" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    let results = results_of(&value, 0);
    assert_eq!(
        results.len(),
        1,
        "the fence content stays in Alpha: {value}"
    );
    assert_eq!(results[0]["title"], "Alpha");
    assert_eq!(results[0]["source"], "fences");
    assert!(
        results[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("fencecode"),
        "snippet comes from the fenced block: {value}"
    );
    // Line hints are present and ordered.
    let line_start = results[0]["lineStart"].as_i64().unwrap();
    let line_end = results[0]["lineEnd"].as_i64().unwrap();
    assert!(line_start >= 1 && line_end >= line_start, "{value}");
}

// ---------------------------------------------------------------------------
// Search ranking / filters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bm25_title_weight_filters_and_porter_stemming() {
    let harness = Harness::new("search");
    harness
        .index("# Needle title\n\nunrelated body", "title-hit", None)
        .await;
    harness
        .index(
            "plain first line\n\nneedle needle body",
            "body-hit",
            Some("text"),
        )
        .await;
    harness
        .index("code body\n\nneedle source", "code-src", Some("code"))
        .await;

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["needle"], "limit": 5 }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    let results = results_of(&value, 0);
    assert!(results.len() >= 2, "{value}");
    assert_eq!(
        results[0]["title"], "Needle title",
        "the 5x title weight must outrank repeated content hits: {value}"
    );

    // Exact source filter.
    let filtered = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["needle"], "source": "body-hit" }),
        )
        .await;
    let value = parse_json(&filtered);
    for hit in results_of(&value, 0) {
        assert_eq!(hit["source"], "body-hit", "{value}");
    }

    // Exact contentType filter.
    let filtered = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["needle"], "contentType": "code" }),
        )
        .await;
    let value = parse_json(&filtered);
    let results = results_of(&value, 0);
    assert_eq!(results.len(), 1, "{value}");
    assert_eq!(results[0]["source"], "code-src");
    assert_eq!(results[0]["contentType"], "code");

    // Porter stemming: "caching" must reach "cached"/"caches".
    harness
        .index(
            "caching strategies: cached values and caches",
            "porter",
            Some("text"),
        )
        .await;
    let stemmed = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["caching"], "source": "porter" }),
        )
        .await;
    let value = parse_json(&stemmed);
    assert!(
        !results_of(&value, 0).is_empty(),
        "porter tokenizer must match cached/caches: {value}"
    );
}

#[tokio::test]
async fn multi_query_dedupes_chunk_ids_across_queries() {
    let harness = Harness::new("dedupe");
    harness
        .index("sharedtoken appears once here", "dedupe", Some("text"))
        .await;

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["sharedtoken", "sharedtoken"] }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    assert_eq!(results_of(&value, 0).len(), 1, "{value}");
    assert_eq!(
        results_of(&value, 1).len(),
        0,
        "the same chunk id must not appear under a later query: {value}"
    );

    // Single-query shorthand.
    let shorthand = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "query": "sharedtoken" }))
        .await;
    assert!(!shorthand.is_error, "{}", shorthand.content);
    assert_eq!(results_of(&parse_json(&shorthand), 0).len(), 1);
}

#[tokio::test]
async fn empty_knowledge_base_returns_index_first_error() {
    let harness = Harness::new("empty-search");
    let result = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": ["anything"] }))
        .await;
    assert!(result.is_error, "an empty base is an error");
    assert!(
        result.content.contains("empty") && result.content.contains("ctx_index"),
        "the error tells the model to index first: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// Directory indexing bounds
// ---------------------------------------------------------------------------

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
    }
    std::fs::write(&path, contents)
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

#[tokio::test]
async fn directory_indexing_is_bounded_and_respects_skips() {
    let harness = Harness::new("walk");
    // Trees must live under the session cwd: `ctx_index` confines paths.
    let root = harness.cwd.join("tree");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.md\nignored_dir/\n").unwrap();
    write_file(&root, "ignored.md", "ignoredmarker");
    write_file(&root, "ignored_dir/inside.md", "ignoreddirmarker");
    write_file(&root, "node_modules/pkg/inside.md", "nodemarker");
    write_file(&root, "src/a.md", "# A\n\na_unique_marker");
    write_file(&root, "src/b.txt", "b text");
    write_file(&root, "src/image.png", "not an allowed extension");
    write_file(&root, "deep/1/2/3/4/too_deep.md", "deep marker");
    write_file(&root, "deep/1/2/3/4/5/too_deep2.md", "deeper marker");

    let result = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "path": root.to_string_lossy(), "source": "tree" }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let indexed = parse_json(&result);
    assert_eq!(
        indexed["indexed"]["files"], 3,
        "src/a.md + src/b.txt + the depth-5 file; node_modules, ignored entries, the depth-6 file and the .png are skipped: {indexed}"
    );

    let found = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["a_unique_marker"], "source": "tree/src/a.md" }),
        )
        .await;
    assert!(!found.is_error, "{}", found.content);
    assert_eq!(results_of(&parse_json(&found), 0).len(), 1);

    for ignored in ["ignoredmarker", "ignoreddirmarker", "nodemarker"] {
        let result = harness
            .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": [ignored] }))
            .await;
        let value = parse_json(&result);
        assert!(
            results_of(&value, 0).is_empty(),
            "`{ignored}` must not be indexed: {value}"
        );
    }
}

#[tokio::test]
async fn directory_indexing_caps_files_at_200() {
    let harness = Harness::new("walk-cap");
    let root = harness.cwd.join("many");
    std::fs::create_dir_all(&root).unwrap();
    for index in 0..205 {
        write_file(
            &root,
            &format!("f{index:03}.md"),
            &format!("# F{index}\n\nbulk {index}"),
        );
    }
    let result = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "path": root.to_string_lossy(), "source": "many" }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let indexed = parse_json(&result);
    assert_eq!(
        indexed["indexed"]["files"], 200,
        "maxFiles defaults to 200: {indexed}"
    );
}

// ---------------------------------------------------------------------------
// Purge / stats
// ---------------------------------------------------------------------------

async fn stats(harness: &Harness) -> Value {
    let result = harness.run(CTX_STATS_TOOL_NAME, json!({})).await;
    assert!(!result.is_error, "{}", result.content);
    parse_json(&result)
}

#[tokio::test]
async fn purge_requires_confirm_and_reports_rows() {
    let harness = Harness::new("purge");
    harness.index("# Keep\n\nkeepbody", "keep", None).await;
    harness.index("# Drop\n\ndropbody", "drop", None).await;
    assert_eq!(stats(&harness).await["chunks"], 2);

    let cancelled = harness.run(CTX_PURGE_TOOL_NAME, json!({})).await;
    assert!(!cancelled.is_error);
    assert_eq!(cancelled.content, "purge cancelled");
    let false_confirm = harness
        .run(CTX_PURGE_TOOL_NAME, json!({ "confirm": false }))
        .await;
    assert_eq!(false_confirm.content, "purge cancelled");
    assert_eq!(
        stats(&harness).await["chunks"],
        2,
        "a cancelled purge changes nothing"
    );

    let scoped = harness
        .run(
            CTX_PURGE_TOOL_NAME,
            json!({ "confirm": true, "source": "drop" }),
        )
        .await;
    assert!(!scoped.is_error, "{}", scoped.content);
    let value = parse_json(&scoped);
    assert_eq!(value["purged"], 1, "{value}");
    let after_scope = stats(&harness).await;
    assert_eq!(after_scope["chunks"], 1);
    assert_eq!(after_scope["sources"], 1);

    let all = harness
        .run(CTX_PURGE_TOOL_NAME, json!({ "confirm": true }))
        .await;
    assert!(!all.is_error, "{}", all.content);
    assert_eq!(parse_json(&all)["purged"], 1);
    let after_all = stats(&harness).await;
    assert_eq!(after_all["chunks"], 0);
    assert_eq!(after_all["sources"], 0);
}

#[tokio::test]
async fn stats_reports_process_counters_and_db_state() {
    let harness = Harness::new("stats");
    let executed = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "print('hello stats')" }),
        )
        .await;
    assert!(!executed.is_error, "{}", executed.content);
    harness
        .index("# Stats\n\nindexed content body", "stats-src", None)
        .await;

    let value = stats(&harness).await;
    assert_eq!(value["executions"], 1, "{value}");
    assert_eq!(value["fileExecutions"], 0, "{value}");
    assert!(value["bytesIn"].as_u64().unwrap() > 0, "{value}");
    assert!(
        value["bytesOut"].as_u64().unwrap() >= "hello stats".len() as u64,
        "{value}"
    );
    assert_eq!(value["chunks"], 1, "{value}");
    assert_eq!(value["sources"], 1, "{value}");
    assert!(
        value["dbPath"]
            .as_str()
            .unwrap()
            .contains("context-mode.db"),
        "{value}"
    );
    assert!(value["dbSizeBytes"].as_u64().unwrap() > 0, "{value}");
}

// ---------------------------------------------------------------------------
// ctx_fetch_and_index (local HTTP server)
// ---------------------------------------------------------------------------

const PAGE: &str = "<!doctype html><html><head><title>Test Page</title>\
<style>.hidden{color:red}</style><script>secretScript()</script></head>\
<body><h1>Remote Heading</h1><p>needlefetch &amp; more text</p>\
<script>moreSecret()</script></body></html>";

fn spawn_html_server(requests: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|error| panic!("bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("addr: {error}"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            requests.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                PAGE.len(),
                PAGE
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{address}/page")
}

#[tokio::test]
async fn fetch_and_index_extracts_text_and_caches_by_ttl() {
    let harness = Harness::new("fetch");
    let requests = Arc::new(AtomicUsize::new(0));
    let url = spawn_html_server(Arc::clone(&requests));

    // First fetch: not cached, metadata only.
    let first = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": url, "source": "remote-doc" }),
        )
        .await;
    assert!(!first.is_error, "{}", first.content);
    let value = parse_json(&first);
    assert_eq!(value["cached"], false, "{value}");
    assert!(value["chunks"].as_u64().unwrap() >= 1, "{value}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    for forbidden in ["secretScript", "moreSecret", "<html", "needlefetch"] {
        assert!(
            !first.content.contains(forbidden),
            "raw page content must not enter the result (`{forbidden}`): {}",
            first.content
        );
    }

    // The raw HTML was converted to text and indexed: no script/style text,
    // entity decoded, heading preserved as the chunk title.
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["needlefetch"], "source": "remote-doc" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    let results = results_of(&value, 0);
    assert_eq!(results.len(), 1, "{value}");
    assert_eq!(results[0]["title"], "Remote Heading", "{value}");
    let snippet = results[0]["snippet"].as_str().unwrap();
    assert!(snippet.contains("needlefetch & more text"), "{snippet}");
    assert!(!snippet.contains("secretScript"), "{snippet}");

    // Cache hit: metadata only, no second request.
    let cached = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": url, "source": "remote-doc" }),
        )
        .await;
    assert!(!cached.is_error, "{}", cached.content);
    assert_eq!(parse_json(&cached)["cached"], true);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a TTL cache hit must not re-fetch"
    );

    // force bypasses the cache.
    let forced = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": url, "source": "remote-doc", "force": true }),
        )
        .await;
    assert!(!forced.is_error, "{}", forced.content);
    assert_eq!(parse_json(&forced)["cached"], false);
    assert_eq!(requests.load(Ordering::SeqCst), 2);

    // ttl: 1 — a cache entry older than one second is stale.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let expired = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": url, "source": "remote-doc", "ttl": 1 }),
        )
        .await;
    assert!(!expired.is_error, "{}", expired.content);
    assert_eq!(parse_json(&expired)["cached"], false);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "an expired TTL entry is re-fetched"
    );

    // ttl: 0 bypasses the cache like force.
    let bypass = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": url, "source": "remote-doc", "ttl": 0 }),
        )
        .await;
    assert!(!bypass.is_error, "{}", bypass.content);
    assert_eq!(parse_json(&bypass)["cached"], false);
    assert_eq!(requests.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn fetch_rejects_non_http_schemes() {
    let harness = Harness::new("fetch-scheme");
    let result = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": "file:///etc/passwd" }),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("http") || result.content.contains("scheme"),
        "got: {}",
        result.content
    );
}
