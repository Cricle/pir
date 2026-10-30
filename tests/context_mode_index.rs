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

async fn source_hits(harness: &Harness, query: &str, source: &str) -> Vec<Value> {
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": query, "source": source }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    results_of(&parse_json(&search), 0)
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

/// The pre-optimization `make_snippet` (char-window over `Vec<char>`), kept
/// verbatim as the behavioral oracle for the allocation-free implementation.
fn reference_snippet(content: &str, terms: &[String], max_chars: usize) -> String {
    let lowered = content.to_lowercase();
    let mut position = None;
    for term in terms {
        let needle = term.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        if let Some(byte_index) = lowered.find(&needle) {
            position = Some(if content.is_char_boundary(byte_index) {
                content[..byte_index].chars().count()
            } else {
                0
            });
            break;
        }
    }
    let chars: Vec<char> = content.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let window = max_chars.max(80);
    let center = position.unwrap_or(0);
    let start = center.saturating_sub(window / 3);
    let end = (start + window).min(chars.len());
    let start = end.saturating_sub(window);
    let mut snippet: String = chars[start..end].iter().collect();
    snippet = snippet.trim().to_owned();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
}

/// The term split `build_match_expression` uses (alphanumeric or `_`).
fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

#[tokio::test]
async fn snippets_match_the_reference_window_for_ascii_and_unicode() {
    let harness = Harness::new("snippet-equivalence");
    let long_prefix = "prefix ".repeat(100);
    let long_suffix = "suffix ".repeat(100);
    let mut turkish = "İstanbul öncesi ".repeat(24);
    turkish.push_str("needle unicode ");
    turkish.push_str(&"κείμενο ".repeat(50));
    let mut wide = "é".repeat(620);
    wide.push_str(" needle wide ");
    wide.push_str(&"é".repeat(620));
    let cases = [
        (
            format!("{long_prefix}Needle in the middle {long_suffix}"),
            "needle",
        ),
        (turkish, "needle"),
        (wide, "needle"),
        (format!("needle {}", "x".repeat(900)), "needle"),
        (format!("{} needle at the end", "é".repeat(700)), "needle"),
        (
            format!("{}UPPER needle here {}", "a".repeat(700), "b".repeat(700)),
            "needle",
        ),
        (
            format!("{}İ needle boundary{}", "ö".repeat(300), "ü".repeat(300)),
            "needle",
        ),
    ];

    for (index, (content, query)) in cases.iter().enumerate() {
        let source = format!("snippet-{index}");
        harness.index(content, &source, Some("text")).await;
        let results = source_hits(&harness, query, &source).await;
        assert_eq!(results.len(), 1, "case {index}");
        // `chunk_plain` stores each source line with a trailing '\n'; the
        // window math (and therefore the starting char) sees that newline.
        let stored = format!("{content}\n");
        let expected = reference_snippet(&stored, &query_terms(query), 500);
        assert_eq!(
            results[0]["snippet"], expected,
            "case {index}: snippet diverged from the reference window"
        );
    }
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
    spawn_html_page(PAGE, requests)
}

fn spawn_html_page(page: &'static str, requests: Arc<AtomicUsize>) -> String {
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
                page.len(),
                page
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{address}/page")
}

const BREAK_PAGE: &str = "<!doctype html><HTML><BODY>\
<h2>Lists &amp; Breaks</h2>\
<p>alpha<br>beta<br/>gamma</p>\
<p>before<br class=\"nope\">after</p>\
<ul><li>item one<li>item two</ul>\
<DIV>omega &lt;tag&gt;</DIV>\
<!-- hiddenneedle -->\
</BODY></HTML>";

#[tokio::test]
async fn fetch_preserves_breaks_lists_and_entities() {
    let harness = Harness::new("fetch-breaks");
    let requests = Arc::new(AtomicUsize::new(0));
    let url = spawn_html_page(BREAK_PAGE, Arc::clone(&requests));

    let fetched = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": url, "source": "break-page" }),
        )
        .await;
    assert!(!fetched.is_error, "{}", fetched.content);
    assert!(!fetched.content.contains("<p>"), "{}", fetched.content);

    let items = source_hits(&harness, "item", "break-page").await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["title"], json!("Lists & Breaks"), "{items:?}");
    let snippet = items[0]["snippet"].as_str().unwrap();
    assert!(snippet.contains("- item one"), "{snippet}");
    assert!(snippet.contains("- item two"), "{snippet}");

    let gamma = source_hits(&harness, "gamma", "break-page").await;
    assert_eq!(gamma.len(), 1, "{gamma:?}");
    assert!(gamma[0]["snippet"].as_str().unwrap().contains("gamma"));

    let before = source_hits(&harness, "before", "break-page").await;
    assert_eq!(before.len(), 1, "{before:?}");
    assert!(
        before[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("before after"),
        "a <br> with attributes is not a line break: {before:?}"
    );

    let omega = source_hits(&harness, "omega", "break-page").await;
    assert_eq!(omega.len(), 1, "{omega:?}");
    assert!(
        omega[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("omega <tag>"),
        "entities must be decoded: {omega:?}"
    );

    assert!(
        source_hits(&harness, "hiddenneedle", "break-page")
            .await
            .is_empty(),
        "comment text must not be indexed"
    );
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
