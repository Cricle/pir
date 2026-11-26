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
    ContextModePlugin, CTX_BATCH_TOOL_NAME, CTX_DOCTOR_TOOL_NAME, CTX_EXECUTE_TOOL_NAME,
    CTX_FETCH_AND_INDEX_TOOL_NAME, CTX_INDEX_TOOL_NAME, CTX_PURGE_TOOL_NAME, CTX_RESUME_TOOL_NAME,
    CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME,
};
use pir::plugins::plugin::{
    AgentPlugin, PluginEvent, PluginRegistrations, RegistrationContext, ToolBackend,
};

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
    /// Direct handle for tests that seed legacy rows the tool APIs normalize.
    db_path: PathBuf,
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    /// Keeps the plugin (and its shared state) for session-scoped tests.
    plugin: ContextModePlugin,
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
        let db_path = plugin.resolved_db_path();
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
            db_path,
            tools,
            plugin,
        }
    }

    /// Push a session identity through the same `session_start` event the
    /// host emits, so journal rows and session-scoped search have an owner.
    async fn set_session(&self, id: &str) {
        let mut payload = json!({ "sessionId": id, "cwd": self.cwd.to_string_lossy() });
        self.plugin
            .on_event(PluginEvent::new("session_start", &mut payload))
            .await;
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

fn sources_of(value: &Value, query_index: usize) -> Vec<String> {
    results_of(value, query_index)
        .iter()
        .filter_map(|hit| hit["source"].as_str().map(str::to_owned))
        .collect()
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
async fn purge_scopes_session_and_project() {
    let harness = Harness::new("purge-session-project");
    harness.set_session("session-purge").await;
    harness
        .index("# Doc\n\nsession_purge_body", "purge-doc", None)
        .await;
    let executed = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "console.log('session_purge_exec')" }),
        )
        .await;
    assert!(!executed.is_error, "{}", executed.content);

    // Session scope removes only the journal rows; the chunk store stays.
    let events = harness.run(CTX_RESUME_TOOL_NAME, json!({})).await;
    assert!(!events.is_error, "{}", events.content);
    assert!(events.content.contains("ctx_index"), "{}", events.content);

    let purged = harness
        .run(
            CTX_PURGE_TOOL_NAME,
            json!({ "confirm": true, "session": "session-purge" }),
        )
        .await;
    assert!(!purged.is_error, "{}", purged.content);
    let value = parse_json(&purged);
    assert!(value["purgedEvents"].as_u64().unwrap_or(0) >= 2, "{value}");
    assert_eq!(stats(&harness).await["chunks"], 1, "chunks survive");
    let resumed = harness.run(CTX_RESUME_TOOL_NAME, json!({})).await;
    assert!(!resumed.is_error, "{}", resumed.content);
    assert!(
        resumed.content.contains("No journal rows"),
        "{}",
        resumed.content
    );

    // Project scope wipes chunks, sources and the journal.
    let project = harness
        .run(
            CTX_PURGE_TOOL_NAME,
            json!({ "confirm": true, "project": true }),
        )
        .await;
    assert!(!project.is_error, "{}", project.content);
    let value = parse_json(&project);
    assert_eq!(value["purged"], 1, "{value}");
    let snapshot = stats(&harness).await;
    assert_eq!(snapshot["chunks"], 0);
    assert_eq!(snapshot["sources"], 0);

    // Multiple scopes are rejected before anything is deleted.
    let conflict = harness
        .run(
            CTX_PURGE_TOOL_NAME,
            json!({ "confirm": true, "project": true, "session": "session-purge" }),
        )
        .await;
    assert!(conflict.is_error, "{}", conflict.content);
    assert!(
        conflict.content.contains("at most one scope"),
        "{}",
        conflict.content
    );
}

#[tokio::test]
async fn purge_blank_or_non_string_source_cannot_widen_to_global() {
    let harness = Harness::new("purge-scope-guard");
    harness.index("# Keep\n\nkeepbody", "keep", None).await;
    harness.index("# Drop\n\ndropbody", "drop", None).await;

    // A present-but-unusable source must be rejected, never treated as
    // "omitted" (which would wipe the whole knowledge base).
    for source in [
        json!(""),
        json!("   "),
        json!(null),
        json!(123),
        json!(["drop"]),
    ] {
        let result = harness
            .run(
                CTX_PURGE_TOOL_NAME,
                json!({ "confirm": true, "source": source }),
            )
            .await;
        assert!(
            result.is_error,
            "source {source} must be rejected, got {}",
            result.content
        );
        assert!(
            result.content.contains("non-empty string"),
            "got: {}",
            result.content
        );
        assert_eq!(
            stats(&harness).await["chunks"],
            2,
            "a rejected purge must not delete anything"
        );
    }

    // Omitting `source` keeps the documented global-scope behavior.
    let all = harness
        .run(CTX_PURGE_TOOL_NAME, json!({ "confirm": true }))
        .await;
    assert!(!all.is_error, "{}", all.content);
    assert_eq!(parse_json(&all)["purged"], 2);
    assert_eq!(stats(&harness).await["chunks"], 0);
}

#[tokio::test]
async fn indexed_source_is_trimmed_so_scoped_search_and_purge_can_address_it() {
    let harness = Harness::new("source-trim");
    harness
        .index("# Doc\n\nneedle body", "  docs  ", Some(" Markdown "))
        .await;

    let snapshot = stats(&harness).await;
    assert_eq!(snapshot["sourceList"][0]["source"], "docs", "{snapshot}");
    assert_eq!(
        snapshot["sourceList"][0]["contentType"], "markdown",
        "{snapshot}"
    );

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "needle", "source": "docs" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    assert_eq!(results_of(&parse_json(&search), 0).len(), 1);

    let purged = harness
        .run(
            CTX_PURGE_TOOL_NAME,
            json!({ "confirm": true, "source": " docs " }),
        )
        .await;
    assert!(!purged.is_error, "{}", purged.content);
    assert_eq!(parse_json(&purged)["purged"], 1);
    assert_eq!(stats(&harness).await["chunks"], 0);
}

#[tokio::test]
async fn search_content_type_filter_is_case_insensitive() {
    let harness = Harness::new("ct-case");
    harness
        .index("# Doc\n\nneedle body", "docs", Some("Markdown"))
        .await;

    for filter in ["markdown", "Markdown", "MARKDOWN", " markdown "] {
        let search = harness
            .run(
                CTX_SEARCH_TOOL_NAME,
                json!({ "query": "needle", "contentType": filter }),
            )
            .await;
        assert!(!search.is_error, "{filter}: {}", search.content);
        assert_eq!(
            results_of(&parse_json(&search), 0).len(),
            1,
            "contentType filter {filter:?} must match the stored lowercase type"
        );
    }
}

#[tokio::test]
async fn search_content_type_filter_matches_legacy_mixed_case_rows() {
    let harness = Harness::new("ct-legacy");
    // Open the DB/schema through the tool path, then seed a pre-migration row
    // whose stored `content_type` was never lowercased.
    harness
        .index("# Fresh\n\nneedle fresh", "fresh", Some("text"))
        .await;
    {
        let connection =
            rusqlite::Connection::open(&harness.db_path).expect("raw context-mode connection");
        connection
            .execute(
                "INSERT INTO chunks \
                 (source, content_type, title, content, line_start, line_end, indexed_at) \
                 VALUES ('legacy', 'Markdown', 'Legacy', 'needle legacy', 1, 1, 0)",
                [],
            )
            .expect("insert legacy row");
        let id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO chunks_fts (rowid, title, content) \
                 VALUES (?1, 'Legacy', 'needle legacy')",
                [id],
            )
            .expect("insert legacy fts row");
    }

    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "needle", "contentType": "markdown" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    let value = parse_json(&search);
    let results = results_of(&value, 0);
    assert_eq!(
        results.len(),
        1,
        "legacy mixed-case row must match: {value}"
    );
    assert_eq!(results[0]["source"], "legacy");
    assert_eq!(results[0]["contentType"], "Markdown");
}

#[tokio::test]
async fn stats_reports_process_counters_and_db_state() {
    let harness = Harness::new("stats");
    let executed = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "console.log('hello stats')" }),
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

#[tokio::test]
async fn stats_reports_session_and_project_journal_usage() {
    let harness = Harness::new("stats-usage");
    harness.set_session("stats-session").await;
    let executed = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "console.log('usage exec')" }),
        )
        .await;
    assert!(!executed.is_error, "{}", executed.content);
    harness
        .index("# Usage\n\nusage body", "usage-src", None)
        .await;

    let value = stats(&harness).await;
    let session = &value["usage"]["session"];
    assert!(session["calls"].as_u64().unwrap_or(0) >= 2, "{value}");
    assert!(session["bytes"].as_u64().unwrap_or(0) > 0, "{value}");
    let kinds: Vec<&str> = session["byKind"]
        .as_array()
        .expect("byKind")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"exec"), "{value}");
    assert!(kinds.contains(&"index"), "{value}");
    assert_eq!(
        value["usage"]["project"], value["usage"]["allTime"],
        "one DB per project, so project and all-time totals agree"
    );
    assert_eq!(
        value["usage"]["project"]["calls"], session["calls"],
        "{value}"
    );
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

// ---------------------------------------------------------------------------
// Upstream v2 surface: scopes, timeline, journal, ctx_resume, ctx_batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_scopes_separate_session_worktree_and_project() {
    let harness = Harness::new("scopes");
    harness.set_session("session-a").await;
    harness
        .index("# A\n\nscope_marker alpha", "doc-a", None)
        .await;

    // A second session indexed from a sibling worktree: its row must stay out
    // of session-a's session scope and out of the cwd's worktree scope, yet
    // remain visible to the project scope.
    let other = harness.cwd.parent().expect("parent").join("other-worktree");
    std::fs::create_dir_all(&other).expect("other cwd");
    harness.set_session("session-b").await;
    let other_index = harness
        .tools
        .get(CTX_INDEX_TOOL_NAME)
        .expect("ctx_index")
        .run(
            &json!({ "content": "# B\n\nscope_marker beta", "source": "doc-b" }),
            &ToolContext::new(other),
        )
        .await;
    assert!(!other_index.is_error, "{}", other_index.content);
    harness.set_session("session-a").await;

    let session = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "scope_marker", "scope": "session", "limit": 10 }),
        )
        .await;
    assert!(!session.is_error, "{}", session.content);
    let session = parse_json(&session);
    let session_sources = sources_of(&session, 0);
    assert_eq!(
        session_sources,
        vec!["doc-a".to_owned()],
        "session scope sees only session-a's rows: {session}"
    );

    let worktree = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "scope_marker", "scope": "worktree", "limit": 10 }),
        )
        .await;
    assert!(!worktree.is_error, "{}", worktree.content);
    assert_eq!(
        sources_of(&parse_json(&worktree), 0),
        vec!["doc-a".to_owned()],
        "worktree scope excludes the sibling worktree"
    );

    let project = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "scope_marker", "scope": "project", "limit": 10 }),
        )
        .await;
    assert!(!project.is_error, "{}", project.content);
    let project_sources = sources_of(&parse_json(&project), 0);
    assert!(
        project_sources.contains(&"doc-a".to_owned()),
        "{project_sources:?}"
    );
    assert!(
        project_sources.contains(&"doc-b".to_owned()),
        "{project_sources:?}"
    );
}

#[tokio::test]
async fn timeline_sort_pages_oldest_first_with_an_after_cursor() {
    let harness = Harness::new("timeline");
    harness.index("# One\n\nfirst_timeline", "one", None).await;
    // Real-time ordering: keep the two stamps distinct (indexed_at is ms).
    tokio::time::sleep(Duration::from_millis(3)).await;
    harness.index("# Two\n\nsecond_timeline", "two", None).await;

    let listing = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": [], "sort": "timeline", "limit": 10 }),
        )
        .await;
    assert!(!listing.is_error, "{}", listing.content);
    let value = parse_json(&listing);
    let results = results_of(&value, 0);
    assert_eq!(results.len(), 2, "{value}");
    assert_eq!(results[0]["source"], "one", "oldest first: {value}");
    assert_eq!(results[1]["source"], "two", "{value}");

    // The reported `indexedAt` is the cursor the next page uses.
    let first_stamp = results[0]["indexedAt"].as_i64().expect("indexedAt");
    assert!(first_stamp > 0, "{value}");
    let next = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": [], "sort": "timeline", "after": first_stamp, "limit": 10 }),
        )
        .await;
    assert!(!next.is_error, "{}", next.content);
    let next_sources = sources_of(&parse_json(&next), 0);
    assert_eq!(next_sources, vec!["two".to_owned()], "after pages past one");

    // `offset` pages within the same window.
    let offset = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": [], "sort": "timeline", "offset": 1, "limit": 10 }),
        )
        .await;
    assert!(!offset.is_error, "{}", offset.content);
    assert_eq!(sources_of(&parse_json(&offset), 0), vec!["two".to_owned()]);
}

#[tokio::test]
async fn ctx_resume_renders_the_journal_and_returns_one_row_by_seq() {
    let harness = Harness::new("resume");
    harness.set_session("journal-session").await;
    harness
        .index("# Doc\n\nresume_marker", "resume-doc", None)
        .await;
    let executed = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "console.log('resume_exec_marker')" }),
        )
        .await;
    assert!(!executed.is_error, "{}", executed.content);

    let block = harness.run(CTX_RESUME_TOOL_NAME, json!({})).await;
    assert!(!block.is_error, "{}", block.content);
    assert!(
        block.content.contains("journal-session"),
        "{}",
        block.content
    );
    assert!(block.content.contains("resume-doc"), "{}", block.content);
    assert!(block.content.contains("ctx_index"), "{}", block.content);
    assert!(block.content.contains("ctx_execute"), "{}", block.content);
    assert!(
        block.content.len() <= 2_048,
        "the block respects RESUME_CAP: {} B",
        block.content.len()
    );

    // Each rendered line is `#<seq> kind/tool...`; the seq fetches the row
    // whole (the pointer form).
    let seq = block
        .content
        .lines()
        .filter_map(|line| line.strip_prefix('#')?.split_whitespace().next())
        .filter(|digits| digits.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|digits| digits.parse::<u64>().ok())
        .next()
        .expect("a rendered seq in the block");
    let row = harness
        .run(CTX_RESUME_TOOL_NAME, json!({ "seq": seq }))
        .await;
    assert!(!row.is_error, "{}", row.content);
    assert!(!row.content.is_empty());

    let missing = harness
        .run(CTX_RESUME_TOOL_NAME, json!({ "seq": 9_999_999u64 }))
        .await;
    assert!(missing.is_error, "unknown seqs are errors");
}

#[tokio::test]
async fn ctx_resume_rejects_an_unknown_session() {
    let harness = Harness::new("resume-no-session");
    let result = harness.run(CTX_RESUME_TOOL_NAME, json!({})).await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("no session id"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn ctx_batch_runs_shell_commands_and_returns_matching_sections() {
    let harness = Harness::new("shell-batch");
    let result = harness
        .run(
            CTX_BATCH_TOOL_NAME,
            json!({
                "commands": ["echo batch_marker_123", "echo second_marker_456"],
                "queries": ["batch_marker_123"]
            }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("batch_marker_123"),
        "the matching section comes back: {}",
        result.content
    );
    assert!(
        result.content.contains("ctx_batch:"),
        "the reply names the batch source: {}",
        result.content
    );
    assert!(
        result.content.contains("2 command(s)"),
        "all commands ran: {}",
        result.content
    );

    // The blocks are searchable afterwards under the batch source.
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "second_marker_456", "scope": "project" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    assert!(
        !results_of(&parse_json(&search), 0).is_empty(),
        "the second command's output was indexed too"
    );
}

#[tokio::test]
async fn ctx_batch_requires_commands_and_queries() {
    let harness = Harness::new("shell-batch-bounds");
    let no_commands = harness
        .run(
            CTX_BATCH_TOOL_NAME,
            json!({ "commands": [], "queries": ["x"] }),
        )
        .await;
    assert!(no_commands.is_error);
    let no_queries = harness
        .run(
            CTX_BATCH_TOOL_NAME,
            json!({ "commands": ["echo x"], "queries": [] }),
        )
        .await;
    assert!(no_queries.is_error);
    let too_many = harness
        .run(
            CTX_BATCH_TOOL_NAME,
            json!({ "commands": vec!["echo x"; 17], "queries": ["x"] }),
        )
        .await;
    assert!(too_many.is_error);
}

// ---------------------------------------------------------------------------
// Per-session retention
//
// The store is fed by hooks, so without bounds a long-lived project grows
// until the disk fills. Every limit is per session: one session can never
// evict another session's rows, and eviction is always oldest-first.
// ---------------------------------------------------------------------------

const TINY_RETENTION: pir::plugins::builtin::context_mode::index::RetentionLimits =
    pir::plugins::builtin::context_mode::index::RetentionLimits {
        journal_max_rows: 3,
        journal_max_bytes: 4 * 1024,
        journal_ttl_secs: 7 * 24 * 60 * 60,
        chunk_max_rows: 2,
        chunk_max_bytes: 8 * 1024,
        fetch_cache_max_rows: 2,
        fetch_cache_max_bytes: 8 * 1024,
    };

/// A harness whose per-session caps are small enough to evict within a test.
struct BoundedHarness {
    inner: Harness,
}

impl BoundedHarness {
    fn new(tag: &str) -> Self {
        let dir = TestDir::new(tag);
        let cwd = dir.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap_or_else(|error| panic!("cwd: {error}"));
        let plugin = ContextModePlugin::with_retention_limits(
            dir.path().join("context-mode.db"),
            TINY_RETENTION,
        );
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
            inner: Harness {
                _dir: dir,
                cwd,
                db_path: plugin.resolved_db_path(),
                tools,
                plugin,
            },
        }
    }
}

/// One journal row, written directly through the store so the test does not
/// pay for a JS sandbox round trip per row.
async fn append_rows(harness: &Harness, session: &str, rows: usize) {
    use pir::plugins::builtin::context_mode::index::JournalInput;
    let store = harness.plugin.index_store();
    for index in 0..rows {
        let body = format!("row {index} body padding padding padding padding padding");
        let bytes = body.len();
        store
            .append_event(
                JournalInput::new(session.to_owned(), "exec", "ctx_execute", body).with_outcome(
                    true,
                    Some(0),
                    bytes,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("append row: {error}"));
    }
}

#[tokio::test]
async fn journal_rows_are_capped_per_session_oldest_first() {
    let harness = BoundedHarness::new("retention-rows").inner;
    harness.set_session("sess-a").await;
    append_rows(&harness, "sess-a", 10).await;

    let store = harness.plugin.index_store();
    let events = store
        .session_events("sess-a".to_owned(), None)
        .await
        .expect("read back");
    assert_eq!(
        events.len(),
        TINY_RETENTION.journal_max_rows,
        "only the newest rows survive the per-session cap"
    );
    // The surviving rows are the newest ones: their bodies name rows 7..9.
    let bodies: Vec<&str> = events.iter().map(|event| event.body.as_str()).collect();
    assert!(
        bodies.iter().any(|body| body.contains("row 9")),
        "{bodies:?}"
    );
    assert!(
        !bodies.iter().any(|body| body.contains("row 0 ")),
        "the oldest row was evicted: {bodies:?}"
    );

    let retention = store
        .retention("sess-a".to_owned())
        .await
        .expect("retention");
    assert_eq!(retention.journal_rows, TINY_RETENTION.journal_max_rows);
    assert!(!retention.over_limit, "the write path trimmed to the cap");
}

#[tokio::test]
async fn one_session_never_evicts_another_sessions_rows() {
    let harness = BoundedHarness::new("retention-isolation").inner;
    harness.set_session("sess-a").await;
    append_rows(&harness, "sess-a", 3).await;
    append_rows(&harness, "sess-b", 10).await;

    let store = harness.plugin.index_store();
    let a = store
        .session_events("sess-a".to_owned(), None)
        .await
        .unwrap();
    let b = store
        .session_events("sess-b".to_owned(), None)
        .await
        .unwrap();
    assert_eq!(a.len(), 3, "session A keeps all of its rows: {a:?}");
    assert_eq!(
        b.len(),
        TINY_RETENTION.journal_max_rows,
        "session B is capped"
    );
}

#[tokio::test]
async fn journal_bytes_are_capped_per_session() {
    let mut limits = TINY_RETENTION;
    // Byte cap below three rows' worth, so the row cap would not trigger.
    limits.journal_max_bytes = 120;
    let max_bytes = limits.journal_max_bytes;
    let dir = TestDir::new("retention-bytes");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let plugin =
        ContextModePlugin::with_retention_limits(dir.path().join("context-mode.db"), limits);
    let store = plugin.index_store();
    use pir::plugins::builtin::context_mode::index::JournalInput;
    for index in 0..6 {
        let body = format!("row {index} padding padding");
        let bytes = body.len();
        store
            .append_event(
                JournalInput::new("sess-bytes".to_owned(), "exec", "ctx_execute", body)
                    .with_outcome(true, Some(0), bytes),
            )
            .await
            .unwrap();
    }
    // The cap measures the stored `body`; the `bytes` column deliberately
    // overstates it (the original output size) and must not drive eviction.
    let events = store
        .session_events("sess-bytes".to_owned(), None)
        .await
        .unwrap();
    let stored = events
        .iter()
        .map(|event| event.body.len() as u64)
        .sum::<u64>();
    assert!(
        stored <= max_bytes,
        "stored journal bodies stay under the per-session cap: {stored} > {max_bytes}"
    );
    assert!(
        events.len() < 6,
        "the oldest rows were evicted to fit the byte cap: {events:?}"
    );
    drop(store);
    drop(plugin);
    drop(dir);
}

#[tokio::test]
async fn stale_journal_rows_expire_per_session() {
    let mut limits = TINY_RETENTION;
    limits.journal_ttl_secs = 1;
    let dir = TestDir::new("retention-ttl");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let plugin =
        ContextModePlugin::with_retention_limits(dir.path().join("context-mode.db"), limits);
    let store = plugin.index_store();
    use pir::plugins::builtin::context_mode::index::JournalInput;
    // Two rows from the store, then backdate the first past the TTL.
    store
        .append_event(JournalInput::new(
            "sess-ttl".to_owned(),
            "exec",
            "ctx_execute",
            "ancient".to_owned(),
        ))
        .await
        .unwrap();
    {
        use rusqlite::Connection;
        let connection = Connection::open(plugin.resolved_db_path()).unwrap();
        connection
            .execute(
                "UPDATE events SET created_at = 1 WHERE body = 'ancient'",
                [],
            )
            .unwrap();
    }
    store
        .append_event(JournalInput::new(
            "sess-ttl".to_owned(),
            "exec",
            "ctx_execute",
            "fresh".to_owned(),
        ))
        .await
        .unwrap();
    let events = store
        .session_events("sess-ttl".to_owned(), None)
        .await
        .unwrap();
    assert_eq!(events.len(), 1, "the stale row expired: {events:?}");
    assert_eq!(events[0].body, "fresh");
    drop(store);
    drop(plugin);
    drop(dir);
}

#[tokio::test]
async fn chunks_are_capped_per_session_oldest_first() {
    let harness = BoundedHarness::new("retention-chunks").inner;
    harness.set_session("sess-chunks").await;
    // Each call indexes one markdown section (~1 chunk) under its own source.
    for index in 0..6 {
        let result = harness
            .run(
                CTX_INDEX_TOOL_NAME,
                json!({
                    "content": format!("# Section {index}\n\nbody {index}"),
                    "source": format!("doc-{index}"),
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
    }

    let stats = stats(&harness).await;
    assert_eq!(
        stats["chunks"],
        json!(TINY_RETENTION.chunk_max_rows),
        "per-session chunk cap holds: {stats}"
    );
    // The newest sections survive; the oldest are gone.
    let newest = source_hits(&harness, "body", "doc-5").await;
    assert!(!newest.is_empty(), "the newest chunk survives");
    let oldest = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "query": "body", "source": "doc-0", "scope": "project" }),
        )
        .await;
    let oldest = parse_json(&oldest);
    assert!(
        results_of(&oldest, 0).is_empty(),
        "the oldest chunk was evicted: {oldest}"
    );
}

#[tokio::test]
async fn doctor_and_stats_report_retention_limits() {
    let harness = BoundedHarness::new("retention-report").inner;
    harness.set_session("sess-report").await;
    append_rows(&harness, "sess-report", 5).await;

    let doctor = harness.run(CTX_DOCTOR_TOOL_NAME, json!({})).await;
    assert!(!doctor.is_error, "{}", doctor.content);
    let value = parse_json(&doctor);
    assert_eq!(
        value["retention"]["limits"]["journalMaxRows"],
        json!(TINY_RETENTION.journal_max_rows),
        "{value}"
    );
    assert_eq!(
        value["retention"]["session"]["journalRows"],
        json!(TINY_RETENTION.journal_max_rows),
        "{value}"
    );

    let stats = stats(&harness).await;
    assert_eq!(
        stats["retention"]["limits"]["chunkMaxRows"],
        json!(TINY_RETENTION.chunk_max_rows),
        "{stats}"
    );
}

#[tokio::test]
async fn the_same_source_label_from_two_sessions_keeps_both() {
    let harness = Harness::new("retention-source-isolation");
    harness.set_session("sess-one").await;
    harness
        .index("# Shared\n\nshared_marker_one", "shared-doc", None)
        .await;

    // A second session re-indexes the same label. Replacement is scoped to
    // (source, session), so the first session's rows must survive.
    harness.set_session("sess-two").await;
    harness
        .index("# Shared\n\nshared_marker_two", "shared-doc", None)
        .await;

    let stats = stats(&harness).await;
    assert_eq!(
        stats["chunks"],
        json!(2),
        "both sessions keep their chunk under the shared label: {stats}"
    );
    for marker in ["shared_marker_one", "shared_marker_two"] {
        let search = harness
            .run(
                CTX_SEARCH_TOOL_NAME,
                json!({ "query": marker, "source": "shared-doc", "scope": "project" }),
            )
            .await;
        assert!(!search.is_error, "{}", search.content);
        assert!(
            !results_of(&parse_json(&search), 0).is_empty(),
            "`{marker}` stays searchable: {}",
            search.content
        );
    }
}

/// The byte caps must measure encoded bytes, not SQLite `LENGTH()` characters:
/// a CJK body is 3 bytes per character, so a character-counted cap would let
/// roughly 3x the intended data through. One row here is ~100 bytes but only
/// ~35 characters, so a character-based cap would keep all three.
#[tokio::test]
async fn journal_byte_cap_counts_utf8_bytes_not_characters() {
    let mut limits = TINY_RETENTION;
    // Room for two rows by BYTES, but all three by CHARACTER count.
    limits.journal_max_bytes = 240;
    let max_bytes = limits.journal_max_bytes;
    let dir = TestDir::new("retention-bytes-utf8");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let plugin =
        ContextModePlugin::with_retention_limits(dir.path().join("context-mode.db"), limits);
    let store = plugin.index_store();
    use pir::plugins::builtin::context_mode::index::JournalInput;
    // 30 CJK characters = 90 UTF-8 bytes each.
    let body = "中".repeat(30);
    for _ in 0..3 {
        store
            .append_event(JournalInput::new(
                "sess-utf8".to_owned(),
                "exec",
                "ctx_execute",
                body.clone(),
            ))
            .await
            .unwrap();
    }
    let events = store
        .session_events("sess-utf8".to_owned(), None)
        .await
        .unwrap();
    let bytes: u64 = events.iter().map(|event| event.body.len() as u64).sum();
    assert!(
        bytes <= max_bytes,
        "the cap is measured in bytes: {bytes} > {max_bytes}"
    );
    assert!(
        events.len() < 3,
        "a character-counted cap would have kept all three rows: {} kept",
        events.len()
    );
    drop(store);
    drop(plugin);
    drop(dir);
}
