//! Hardening tests for the native `context-mode` builtin, from the adversarial
//! review (`logs/perf/context-mode-review.md`):
//!
//! - `ctx_index` path confinement (absolute outside paths, `..`, symlink
//!   escapes, extension allow-list, inside-cwd success),
//! - write-time output capping (a long print loop never buffers past the cap),
//! - `ctx_search` query count/length bounds,
//! - bounded directory walking (a pathological subtree is never listed),
//! - fetch redirect policy (`file://` redirects rejected, local redirects
//!   followed, hop cap),
//! - database path selection from the pushed session cwd,
//! - the Python-lock starvation valve policy.
//!
//! Hermetic: temp DBs via `with_db_path`/`with_db_base_dir`; the fetch tests
//! serve their own pages from loopback listeners.

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
use pir::extensions::SessionContextSnapshot;
use pir::plugins::builtin::{
    ContextModePlugin, CTX_EXECUTE_TOOL_NAME, CTX_FETCH_AND_INDEX_TOOL_NAME, CTX_INDEX_TOOL_NAME,
    CTX_SEARCH_TOOL_NAME, MAX_OUTPUT_CHARS,
};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// The embedded RustPython VM serializes user code behind a process-wide
/// execution lock (`src/plugins/builtin/python_vm.rs`), and cargo runs this
/// binary's tests concurrently. A call that has to queue behind another test's
/// long script burns its own deadline there (default 30s for `ctx_execute`),
/// which is what made the write-cap tests flaky under load. Hold this gate for
/// one whole tool call so every call's budget starts only once the shared
/// interpreter is free.
static PYTHON_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Self-cleaning temp directory.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-context-hardening-{tag}-{}-{}",
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
        Self { dir, cwd, tools }
    }

    async fn run(&self, name: &str, args: Value) -> ToolResult {
        // Only the exec tools reach the shared interpreter, but every test
        // here is cheap except the two write-cap scripts; serializing one
        // tool call at a time is the smallest gate that keeps the shared
        // lock out of the per-call deadline.
        let _serial = PYTHON_TEST_LOCK.lock().await;
        let tool = self
            .tools
            .get(name)
            .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
            .clone();
        tool.run(&args, &ToolContext::new(self.cwd.clone())).await
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }
}

fn parse_json(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("expected JSON result, got `{}` ({error})", result.content))
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
    }
    std::fs::write(&path, contents)
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

// ---------------------------------------------------------------------------
// ctx_index: confinement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_rejects_absolute_path_outside_cwd() {
    let harness = Harness::new("index-abs-outside");
    let outside = harness.dir().join("outside.txt");
    std::fs::write(&outside, "OUTSIDE_SECRET_MARKER")
        .unwrap_or_else(|error| panic!("fixture: {error}"));

    let result = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "path": outside.to_string_lossy() }),
        )
        .await;
    assert!(
        result.is_error,
        "an absolute path outside the session cwd must be rejected: {}",
        result.content
    );
    assert!(
        result.content.contains("file access blocked")
            && result.content.contains("outside the project root"),
        "clear confinement error expected, got: {}",
        result.content
    );

    // Nothing was read or stored, so the marker is not searchable.
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["OUTSIDE_SECRET_MARKER"] }),
        )
        .await;
    assert!(
        search.is_error,
        "the knowledge base stays empty: {}",
        search.content
    );
}

#[tokio::test]
async fn index_rejects_parent_directory_escape() {
    let harness = Harness::new("index-dotdot");
    std::fs::write(harness.dir().join("outside.txt"), "dotdot secret")
        .unwrap_or_else(|error| panic!("fixture: {error}"));

    for path in ["../outside.txt", "sub/../../outside.txt"] {
        let result = harness
            .run(CTX_INDEX_TOOL_NAME, json!({ "path": path }))
            .await;
        assert!(
            result.is_error,
            "`{path}` must be rejected: {}",
            result.content
        );
        assert!(
            result.content.contains("file access blocked")
                && result.content.contains("outside the project root"),
            "`{path}`: {}",
            result.content
        );
    }
}

#[test]
fn index_rejects_symlink_escape_when_supported() {
    let harness = Harness::new("index-symlink");
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
        // Creating symlinks needs privileges on Windows; the traversal and
        // absolute-path tests still cover the rejection path.
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("runtime: {error}"));
    let result = runtime.block_on(harness.run(CTX_INDEX_TOOL_NAME, json!({ "path": "link.txt" })));
    assert!(
        result.is_error,
        "symlink escape must be rejected: {}",
        result.content
    );
    assert!(
        result.content.contains("file access blocked")
            && result.content.contains("outside the project root"),
        "got: {}",
        result.content
    );
}

#[tokio::test]
async fn index_directory_outside_cwd_is_rejected() {
    let harness = Harness::new("index-dir-outside");
    let outside = harness.dir().join("outside-tree");
    write_file(&outside, "doc.md", "# Outside\n\noutside tree marker");

    let result = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "path": outside.to_string_lossy() }),
        )
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("file access blocked")
            && result.content.contains("outside the project root"),
        "got: {}",
        result.content
    );
}

#[tokio::test]
async fn index_inside_cwd_file_succeeds_and_extension_allow_list_applies() {
    let harness = Harness::new("index-inside");
    write_file(
        &harness.cwd,
        "docs/notes.md",
        "# Notes\n\ninside_cwd_marker body",
    );
    write_file(&harness.cwd, "secret.bin", "binary payload");
    write_file(&harness.cwd, "no_extension", "credential-like payload");

    // Relative path inside cwd: accepted and searchable.
    let indexed = harness
        .run(CTX_INDEX_TOOL_NAME, json!({ "path": "docs/notes.md" }))
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);
    assert_eq!(parse_json(&indexed)["files"], 1);
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["inside_cwd_marker"], "source": "docs/notes.md" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    assert_eq!(
        parse_json(&search)["queries"][0]["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Absolute path that canonicalizes inside cwd: also accepted.
    let absolute = harness.cwd.join("docs/notes.md");
    let indexed = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "path": absolute.to_string_lossy() }),
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);

    // Single files must use the same extension allow-list as the walk.
    for path in ["secret.bin", "no_extension"] {
        let rejected = harness
            .run(CTX_INDEX_TOOL_NAME, json!({ "path": path }))
            .await;
        assert!(
            rejected.is_error,
            "`{path}` must be rejected: {}",
            rejected.content
        );
        assert!(
            rejected.content.contains("unsupported file extension"),
            "`{path}`: {}",
            rejected.content
        );
    }

    // Inline content is unaffected by the file extension rule.
    let inline = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "content": "# Inline\n\ninline marker", "source": "inline-src" }),
        )
        .await;
    assert!(!inline.is_error, "{}", inline.content);
}

// ---------------------------------------------------------------------------
// ctx_search: bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_rejects_too_many_queries() {
    let harness = Harness::new("search-count");
    let indexed = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "content": "# Bounds\n\nquery budget marker", "source": "bounds" }),
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);

    let too_many: Vec<String> = (0..33).map(|index| format!("query{index}")).collect();
    let rejected = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": too_many }))
        .await;
    assert!(
        rejected.is_error,
        "33 queries must be rejected: {}",
        rejected.content
    );
    assert!(
        rejected.content.contains("at most 32 queries"),
        "got: {}",
        rejected.content
    );

    // Exactly 32 is allowed.
    let allowed: Vec<String> = (0..32).map(|index| format!("query{index}")).collect();
    let accepted = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": allowed }))
        .await;
    assert!(!accepted.is_error, "{}", accepted.content);
}

#[tokio::test]
async fn search_rejects_oversized_query() {
    let harness = Harness::new("search-length");
    let indexed = harness
        .run(
            CTX_INDEX_TOOL_NAME,
            json!({ "content": "# Bounds\n\nquery length marker", "source": "bounds" }),
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);

    let oversized = "a".repeat(513);
    let rejected = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": [oversized] }))
        .await;
    assert!(rejected.is_error, "{}", rejected.content);
    assert!(
        rejected.content.contains("limited to 512 bytes"),
        "got: {}",
        rejected.content
    );

    // Exactly 512 bytes is allowed (no matches, but not an error).
    let at_limit = "a".repeat(512);
    let accepted = harness
        .run(CTX_SEARCH_TOOL_NAME, json!({ "queries": [at_limit] }))
        .await;
    assert!(!accepted.is_error, "{}", accepted.content);
}

// ---------------------------------------------------------------------------
// Directory walk: bounded collection
// ---------------------------------------------------------------------------

#[test]
fn walk_stops_before_listing_a_pathological_subtree() {
    let dir = TestDir::new("walk-probe");
    let root = dir.path().join("tree");
    std::fs::create_dir_all(&root).unwrap();
    // 200 allowed files at the root fill the default budget exactly.
    for index in 0..200 {
        write_file(
            &root,
            &format!("f{index:03}.md"),
            &format!("# F{index}\n\nbulk {index}"),
        );
    }
    // A "pathological" subtree that would be expensive to list. The walk must
    // reach the file cap first and never descend into it.
    for dir_index in 0..20 {
        for file_index in 0..10 {
            write_file(
                &root,
                &format!("zzz_pathological/d{dir_index:02}/f{file_index:02}.md"),
                "pathological body",
            );
        }
    }

    let (files, directories, entries) =
        ContextModePlugin::walk_directory_probe(&root, 200).expect("walk");
    assert_eq!(files, 200, "the file budget is filled exactly");
    assert_eq!(
        directories, 1,
        "the walk must stop before listing the pathological subtree"
    );
    assert!(
        entries <= 201,
        "only the root's 201 entries (200 files + 1 dir) may be read, got {entries}"
    );
}

// ---------------------------------------------------------------------------
// ctx_fetch_and_index: redirect policy
// ---------------------------------------------------------------------------

const PAGE: &str = "<!doctype html><html><body><h1>Local Page</h1>\
<p>redirectneedle body</p></body></html>";

/// Minimal one-request-at-a-time HTTP server. `handler` receives the request
/// path and returns `(status_line, extra_headers, body)`.
fn spawn_server<F>(handler: F) -> String
where
    F: Fn(&str) -> (String, String, String) + Send + Sync + 'static,
{
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
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_owned();
            let (status, headers, body) = handler(&path);
            let response = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn fetch_rejects_redirect_to_file_scheme() {
    let harness = Harness::new("fetch-file-redirect");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let url = spawn_server(move |_path| {
        counter.fetch_add(1, Ordering::SeqCst);
        (
            "302 Found".to_owned(),
            "Location: file:///etc/passwd\r\n".to_owned(),
            String::new(),
        )
    });

    let result = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": format!("{url}/redirect") }),
        )
        .await;
    assert!(
        result.is_error,
        "a file:// redirect must be rejected: {}",
        result.content
    );
    assert!(
        result.content.contains("redirect") && result.content.contains("file"),
        "the error names the redirect scheme, got: {}",
        result.content
    );
    assert!(
        result.content.contains("http") && result.content.contains("https"),
        "the allowed schemes are named, got: {}",
        result.content
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the fetcher must not issue a follow-up request for the blocked hop"
    );
}

#[tokio::test]
async fn fetch_follows_local_http_redirect_and_caps_redirects() {
    let harness = Harness::new("fetch-redirect-ok");
    let url = spawn_server(|path| {
        if path == "/start" {
            (
                "302 Found".to_owned(),
                "Location: /page\r\n".to_owned(),
                String::new(),
            )
        } else {
            (
                "200 OK".to_owned(),
                "Content-Type: text/html; charset=utf-8\r\n".to_owned(),
                PAGE.to_owned(),
            )
        }
    });

    // A same-origin http redirect still works, and the local page is indexed.
    let followed = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": format!("{url}/start"), "source": "redirected" }),
        )
        .await;
    assert!(!followed.is_error, "{}", followed.content);
    let value = parse_json(&followed);
    assert_eq!(value["cached"], false, "{value}");
    let search = harness
        .run(
            CTX_SEARCH_TOOL_NAME,
            json!({ "queries": ["redirectneedle"], "source": "redirected" }),
        )
        .await;
    assert!(!search.is_error, "{}", search.content);
    assert_eq!(
        parse_json(&search)["queries"][0]["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Redirect loops stop after the hop cap with a clear error.
    let loop_url = spawn_server(|_path| {
        (
            "302 Found".to_owned(),
            "Location: /loop\r\n".to_owned(),
            String::new(),
        )
    });
    let looped = harness
        .run(
            CTX_FETCH_AND_INDEX_TOOL_NAME,
            json!({ "url": format!("{loop_url}/loop") }),
        )
        .await;
    assert!(looped.is_error, "{}", looped.content);
    assert!(
        looped.content.contains("redirect limit"),
        "got: {}",
        looped.content
    );
}

// ---------------------------------------------------------------------------
// Database path: pushed session cwd
// ---------------------------------------------------------------------------

fn snapshot_with_cwd(cwd: &Path) -> SessionContextSnapshot {
    SessionContextSnapshot {
        cwd: Some(cwd.to_string_lossy().into_owned()),
        ..SessionContextSnapshot::default()
    }
}

#[test]
fn database_path_prefers_the_pushed_session_cwd() {
    let dir = TestDir::new("db-cwd");
    let base = dir.path().join("agent-base");
    let cwd_a = dir.path().join("project-a");
    let cwd_b = dir.path().join("project-b");
    for cwd in [&cwd_a, &cwd_b] {
        std::fs::create_dir_all(cwd).unwrap();
    }

    let plugin_a = ContextModePlugin::with_db_base_dir(&base);
    let plugin_b = ContextModePlugin::with_db_base_dir(&base);
    let plugin_a2 = ContextModePlugin::with_db_base_dir(&base);
    plugin_a.on_session_context(&snapshot_with_cwd(&cwd_a));
    plugin_b.on_session_context(&snapshot_with_cwd(&cwd_b));
    plugin_a2.on_session_context(&snapshot_with_cwd(&cwd_a));

    let path_a = plugin_a.resolved_db_path();
    let path_b = plugin_b.resolved_db_path();
    let path_a2 = plugin_a2.resolved_db_path();

    // Two projects in one process get separate databases...
    assert_ne!(
        path_a, path_b,
        "different session cwds must resolve to different database files"
    );
    // ...while the same cwd is stable across plugin instances.
    assert_eq!(path_a, path_a2);
    // Both live under the injected base, never the real agent dir.
    let expected_dir = base.join("context-mode");
    for path in [&path_a, &path_b] {
        assert!(
            path.starts_with(&expected_dir),
            "{} must be under {}",
            path.display(),
            expected_dir.display()
        );
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("db")
        );
    }

    // Without a push the process-cwd fallback applies (and differs from the
    // pushed temp cwd in a cargo test process).
    let fallback = ContextModePlugin::with_db_base_dir(&base).resolved_db_path();
    assert_ne!(fallback, path_a);
    assert!(fallback.starts_with(&expected_dir));

    // An explicit path always wins over the pushed cwd.
    let explicit_path = dir.path().join("explicit.db");
    let explicit = ContextModePlugin::with_db_path(&explicit_path);
    explicit.on_session_context(&snapshot_with_cwd(&cwd_b));
    assert_eq!(explicit.resolved_db_path(), explicit_path);
}

// ---------------------------------------------------------------------------
// Python lock starvation valve policy
// ---------------------------------------------------------------------------

#[test]
fn python_exec_lock_starvation_valve_allows_two_deadline_ceiling() {
    let threshold = ContextModePlugin::python_exec_lock_wedged_after();
    assert_eq!(
        threshold,
        Duration::from_secs(240),
        "the valve must allow 2x the 120s hard deadline ceiling"
    );
    assert!(threshold >= Duration::from_secs(2 * 120));
}

// ---------------------------------------------------------------------------
// Write-time output cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn execute_print_loop_is_capped_at_write_time() {
    let harness = Harness::new("cap-loop");
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({
                "code": "for _ in range(500000):\n    print(\"x\" * 100)",
                "timeout": 120
            }),
        )
        .await;
    let preview: String = result.content.chars().take(500).collect();
    assert!(
        !result.is_error,
        "the loop must complete (and stay capped), got: {preview}"
    );
    assert!(
        result.content.len() > MAX_OUTPUT_CHARS,
        "the cap itself was reached, got {} chars",
        result.content.len()
    );
    assert!(
        result.content.len() <= MAX_OUTPUT_CHARS + 100,
        "never more than cap + notice, got {} chars",
        result.content.len()
    );
    assert!(result.content.starts_with('x'));
    assert!(result.content.contains("output truncated at 1 MiB"));

    // The session survives and serves the next call.
    let after = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "print('still alive')" }),
        )
        .await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(after.content.trim(), "still alive");
}

#[tokio::test]
async fn execute_single_huge_write_is_capped() {
    let harness = Harness::new("cap-single");
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({ "code": "print('y' * 1_500_000)" }),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.len() > MAX_OUTPUT_CHARS);
    assert!(
        result.content.len() <= MAX_OUTPUT_CHARS + 100,
        "cap + notice, got {} chars",
        result.content.len()
    );
    assert!(result.content.starts_with('y'));
    assert!(result.content.ends_with("[output truncated at 1 MiB] ..."));
}

#[tokio::test]
async fn execute_stderr_is_capped_at_write_time() {
    let harness = Harness::new("cap-stderr");
    let result = harness
        .run(
            CTX_EXECUTE_TOOL_NAME,
            json!({
                "code": "import sys\nsys.stderr.write('z' * 1_500_000)\n1 / 0"
            }),
        )
        .await;
    assert!(result.is_error, "the ZeroDivisionError is still reported");
    assert!(
        result.content.len() <= MAX_OUTPUT_CHARS + 100,
        "stderr is capped at write time, got {} chars",
        result.content.len()
    );
    assert!(result.content.starts_with('z'));
    assert!(result.content.contains("output truncated at 1 MiB"));
}
