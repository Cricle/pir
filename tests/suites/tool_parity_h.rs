//! Lane H parity tests for the core tools:
//!
//! * per-tool `constrainedSampling` (only read/bash/edit/write prefer strict);
//! * `PI_SESSION_*` environment injection through the shell tools;
//! * read path compatibility variants (`@file`, `~`, Unicode spaces,
//!   macOS NFD/curly-quote/AM-PM forms) *before* workspace confinement;
//! * read line counting (upstream's trailing-empty split piece) and grep's
//!   byte-only truncation boundary;
//! * edit's required `edits` schema while the hidden `replaceAll` stays
//!   honored;
//! * pluggable `*Operations` backends for every core tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    declared_tool_definitions, AgentTool, BoxFuture, ToolContext, ToolRegistry, ToolSessionEnv,
};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{
    ConstrainedSamplingConfig, Model, ModelCost, ModelCostRates, StrictPreference, ThinkingLevel,
};
use pir::coding_agent::tools::{
    register_core_tools, BashExecOptions, BashOperations, BashTool, EditOperations, EditTool,
    FindGlobOptions, FindOperations, FindTool, GrepOperations, GrepTool, LsOperations, LsTool,
    PowerShellTool, ReadOperations, ReadTool, WriteOperations, WriteTool,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_workspace(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "pir-h-{label}-{nonce}-{sequence}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create workspace");
    root
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

async fn run_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    cwd: &Path,
) -> ToolResult {
    registry
        .get(name)
        .unwrap_or_else(|| panic!("{name} registered"))
        .run(&args, &ToolContext::new(cwd))
        .await
}

struct BareTool;

impl AgentTool for BareTool {
    fn name(&self) -> &'static str {
        "bare_tool"
    }
    fn description(&self) -> &'static str {
        "a tool without a sampling constraint"
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

// ---------------------------------------------------------------------------
// Per-tool constrained sampling (upstream read.ts:101, bash.ts:260,
// edit.ts:156, write.ts:57)
// ---------------------------------------------------------------------------

#[test]
fn constrained_sampling_is_declared_only_by_the_four_core_tools() {
    let registry = registry();
    let prefer = Some(ConstrainedSamplingConfig::JsonSchema {
        strict: StrictPreference::Prefer,
    });
    for name in ["read", "bash", "edit", "write"] {
        assert_eq!(
            registry
                .get(name)
                .expect("registered")
                .constrained_sampling(),
            prefer,
            "{name} must prefer strict JSON-schema sampling"
        );
    }
    // Everything else — including the other built-ins and extension/custom
    // tools — stays unconstrained.
    for name in ["powershell", "ls", "grep", "find"] {
        assert_eq!(
            registry
                .get(name)
                .expect("registered")
                .constrained_sampling(),
            None,
            "{name} must not declare constrained sampling"
        );
    }
    let bare = BareTool;
    assert_eq!(bare.constrained_sampling(), None);
}

#[test]
fn declared_and_plain_definitions_use_the_per_tool_constraint() {
    let registry = registry();
    let declared: Vec<Arc<dyn AgentTool>> = registry.iter().cloned().collect();
    let definitions = declared_tool_definitions(
        &declared,
        &BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    );
    let by_name = |name: &str| {
        definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
            .constrained_sampling
            .clone()
    };
    let prefer = Some(ConstrainedSamplingConfig::JsonSchema {
        strict: StrictPreference::Prefer,
    });
    assert_eq!(by_name("read"), prefer);
    assert_eq!(by_name("bash"), prefer);
    assert_eq!(by_name("edit"), prefer);
    assert_eq!(by_name("write"), prefer);
    assert_eq!(by_name("ls"), None);
    assert_eq!(by_name("powershell"), None);

    // The plain registry conversion follows the same rule.
    let plain = pir::coding_agent::tools::tool_definitions(&registry);
    let plain_read = plain
        .iter()
        .find(|definition| definition.name == "read")
        .expect("read definition");
    assert_eq!(plain_read.constrained_sampling, prefer);
    let plain_ls = plain
        .iter()
        .find(|definition| definition.name == "ls")
        .expect("ls definition");
    assert_eq!(plain_ls.constrained_sampling, None);
}

// ---------------------------------------------------------------------------
// read path compatibility variants
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_normalizes_at_prefix_unicode_spaces_and_macos_variants() {
    let root = temp_workspace("read-paths");
    let registry = registry();

    std::fs::write(root.join("at.txt"), "at-content").expect("at fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "@at.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "at-content");

    // U+00A0 in the input is a user-visible space typo for a regular space.
    std::fs::write(root.join("hello world.txt"), "space-content").expect("space fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "hello\u{00A0}world.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "space-content");

    // macOS stores screenshot names with U+202F before AM/PM.
    std::fs::write(root.join("Shot 1.00.00\u{202F}PM.txt"), "ampm").expect("ampm fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "Shot 1.00.00 PM.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "ampm");

    // macOS stores decomposed (NFD) filenames; the model types composed NFC.
    std::fs::write(root.join("e\u{0301}.txt"), "nfd").expect("nfd fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "é.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "nfd");

    // macOS screenshot names use U+2019; users type U+0027.
    std::fs::write(root.join("Capture d\u{2019}écran.txt"), "curly").expect("curly fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "Capture d'écran.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "curly");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn read_tilde_expands_but_an_outside_home_is_still_refused() {
    let root = temp_workspace("read-tilde");
    let registry = registry();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let Some(home) = home else {
        let _ = std::fs::remove_dir_all(&root);
        return;
    };
    if home.starts_with(&root) {
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "~/pir-h-definitely-missing.txt"}),
        &root,
    )
    .await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("escapes the workspace root"),
        "tilde must expand before confinement, then be refused: {}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn read_path_variants_never_escape_the_workspace() {
    let root = temp_workspace("read-variants-escape");
    let registry = registry();
    // A sibling file outside the workspace whose name a curly-quote variant of
    // an inside path would name: the variant must still be confined.
    let outside = root.parent().expect("parent").join(format!(
        "{}-outside.txt",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&outside, "outside").expect("outside fixture");
    let relative_escape = format!(
        "../{}-outside.txt",
        root.file_name().unwrap().to_string_lossy()
    );
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": relative_escape}),
        &root,
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("escapes the workspace root"),
        "{}",
        result.content
    );
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// read line counting (upstream `text.split("\n")`, trailing empty included)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_counts_the_trailing_split_piece_like_upstream() {
    let root = temp_workspace("read-lines");
    let registry = registry();
    std::fs::write(root.join("lines.txt"), "one\ntwo\nthree\n").expect("fixture");

    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "one\ntwo\nthree\n");

    // Offset 4 addresses the trailing empty piece: valid and empty.
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt", "offset": 4}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "");

    // Offset 5 is beyond `allLines.length`.
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt", "offset": 5}),
        &root,
    )
    .await;
    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("Offset 5 is beyond end of file (4 lines total)"),
        "{}",
        result.content
    );

    // A user limit counts the trailing piece as remaining content.
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt", "offset": 2, "limit": 1}),
        &root,
    )
    .await;
    assert!(
        result
            .content
            .contains("[2 more lines in file. Use offset=3 to continue.]"),
        "{}",
        result.content
    );

    // An empty file is one (empty) line, exactly like `"".split("\n")`.
    std::fs::write(root.join("empty.txt"), "").expect("empty fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "empty.txt"}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "empty.txt", "offset": 2}),
        &root,
    )
    .await;
    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("Offset 2 is beyond end of file (1 lines total)"),
        "{}",
        result.content
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// grep truncation boundary: byte-only, no 2000-line cutoff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_does_not_truncate_at_2000_lines() {
    let root = temp_workspace("grep-lines");
    let registry = registry();
    // 2500 short matches (~13 bytes each) stay under the 50KB byte budget.
    let content: String = (0..2500).map(|_| "n\n").collect();
    std::fs::write(root.join("m.txt"), content).expect("grep fixture");

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "n", "path": ".", "limit": 3000}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let matches = result
        .content
        .lines()
        .filter(|line| line.starts_with("m.txt:"))
        .count();
    assert_eq!(matches, 2500, "all matches below the byte limit survive");
    assert!(
        !result.content.contains("2000 lines limit"),
        "upstream grep has no line cutoff: {}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn grep_truncates_by_bytes_with_upstream_max_lines() {
    let root = temp_workspace("grep-bytes");
    let registry = registry();
    let line = format!("needle{}\n", "x".repeat(60));
    let content: String = (0..1000).map(|_| line.clone()).collect();
    std::fs::write(root.join("big.txt"), content).expect("grep fixture");

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "needle", "path": ".", "limit": 2000}),
        &root,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("50.0KB limit reached"),
        "expected byte truncation notice: {}",
        result.content
    );
    let details = result.details.expect("truncation details");
    let truncation = &details["truncation"];
    assert_eq!(truncation["truncated"], true);
    assert_eq!(truncation["truncatedBy"], "bytes");
    // Upstream passes `Number.MAX_SAFE_INTEGER` as maxLines.
    assert_eq!(truncation["maxLines"], 9_007_199_254_740_991u64);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// edit schema: `edits` required, `replaceAll` still honored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_requires_edits_and_keeps_hidden_replace_all() {
    let root = temp_workspace("edit-schema");
    let registry = registry();
    let edit = registry.get("edit").expect("edit registered");
    let parameters = edit.parameters().expect("schema");
    assert_eq!(parameters["required"], serde_json::json!(["path", "edits"]));

    // Missing edits: upstream's validateEditInput wording.
    std::fs::write(root.join("f.txt"), "a a a\n").expect("fixture");
    let result = edit
        .run(
            &serde_json::json!({"path": "f.txt"}),
            &ToolContext::new(&root),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("Edit tool input is invalid. edits must contain at least one replacement."),
        "{}",
        result.content
    );

    // Hidden replaceAll still replaces every occurrence.
    let result = edit
        .run(
            &serde_json::json!({
                "path": "f.txt",
                "edits": [{"oldText": "a", "newText": "b", "replaceAll": true}]
            }),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).expect("read result"),
        "b b b\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Pluggable operations backends
// ---------------------------------------------------------------------------

struct FakeRead {
    bytes: Vec<u8>,
    mime: Option<&'static str>,
    seen: Arc<Mutex<Vec<PathBuf>>>,
}

impl ReadOperations for FakeRead {
    fn read_file<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<Vec<u8>>> {
        let bytes = self.bytes.clone();
        let seen = Arc::clone(&self.seen);
        let path = absolute_path.to_path_buf();
        Box::pin(async move {
            seen.lock().expect("seen").push(path);
            Ok(bytes)
        })
    }
    fn access<'a>(&'a self, _absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn detect_image_mime_type<'a>(
        &'a self,
        _absolute_path: &'a Path,
    ) -> BoxFuture<'a, Option<&'static str>> {
        Box::pin(async move { self.mime })
    }
}

#[tokio::test]
async fn read_with_operations_uses_the_injected_backend() {
    let root = temp_workspace("ops-read");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool = ReadTool::with_operations(Arc::new(FakeRead {
        bytes: b"virtual\ntext\n".to_vec(),
        mime: None,
        seen: Arc::clone(&seen),
    }));
    let result = tool
        .run(
            &serde_json::json!({"path": "@virtual.txt"}),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "virtual\ntext\n");
    let seen = seen.lock().expect("seen");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0],
        root.canonicalize()
            .expect("canonical root")
            .join("virtual.txt")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[derive(Default)]
struct FakeWrite {
    writes: Mutex<Vec<(PathBuf, String)>>,
    mkdirs: Mutex<Vec<PathBuf>>,
}

impl WriteOperations for FakeWrite {
    fn write_file<'a>(
        &'a self,
        absolute_path: &'a Path,
        content: &'a str,
    ) -> BoxFuture<'a, std::io::Result<()>> {
        let path = absolute_path.to_path_buf();
        let content = content.to_owned();
        Box::pin(async move {
            self.writes.lock().expect("writes").push((path, content));
            Ok(())
        })
    }
    fn mkdir<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        let dir = dir.to_path_buf();
        Box::pin(async move {
            self.mkdirs.lock().expect("mkdirs").push(dir);
            Ok(())
        })
    }
}

/// Upstream `write.ts:11-14`: the parameter descriptions are model-facing
/// schema text, so they are pinned byte-for-byte (a wording drift is a wire
/// change, not a comment change).
#[test]
fn write_declares_the_upstream_parameter_schema() {
    let tool = WriteTool::new();
    assert_eq!(
        tool.parameters().expect("write schema"),
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    );
}

#[tokio::test]
async fn write_with_operations_uses_the_injected_backend() {
    let root = temp_workspace("ops-write");
    let operations = Arc::new(FakeWrite::default());
    let tool = WriteTool::with_operations(operations.clone());
    let result = tool
        .run(
            &serde_json::json!({"path": "sub/out.txt", "content": "hello"}),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "Successfully wrote to sub/out.txt");
    let writes = operations.writes.lock().expect("writes");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].1, "hello");
    assert_eq!(
        operations.mkdirs.lock().expect("mkdirs").as_slice(),
        &[root.canonicalize().expect("root").join("sub")]
    );
    let _ = std::fs::remove_dir_all(&root);
}

struct FakeEdit {
    content: Mutex<String>,
}

impl EditOperations for FakeEdit {
    fn read_file<'a>(
        &'a self,
        _absolute_path: &'a Path,
    ) -> BoxFuture<'a, std::io::Result<Vec<u8>>> {
        Box::pin(async move { Ok(self.content.lock().expect("content").clone().into_bytes()) })
    }
    fn write_file<'a>(
        &'a self,
        _absolute_path: &'a Path,
        content: &'a str,
    ) -> BoxFuture<'a, std::io::Result<()>> {
        let content = content.to_owned();
        Box::pin(async move {
            *self.content.lock().expect("content") = content;
            Ok(())
        })
    }
    fn access<'a>(&'a self, _absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn edit_with_operations_uses_the_injected_backend() {
    let root = temp_workspace("ops-edit");
    let operations = Arc::new(FakeEdit {
        content: Mutex::new("hello world\n".to_owned()),
    });
    let tool = EditTool::with_operations(operations.clone());
    let result = tool
        .run(
            &serde_json::json!({
                "path": "virtual.txt",
                "edits": [{"oldText": "world", "newText": "there"}]
            }),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("Successfully replaced 1 block(s)"));
    assert_eq!(
        operations.content.lock().expect("content").as_str(),
        "hello there\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

struct FakeBash {
    output: Vec<u8>,
    outcome: Result<Option<i64>, String>,
    env: Mutex<Option<BTreeMap<String, Option<String>>>>,
}

impl FakeBash {
    fn new(output: &[u8], outcome: Result<Option<i64>, String>) -> Self {
        Self {
            output: output.to_vec(),
            outcome,
            env: Mutex::new(None),
        }
    }
}

impl BashOperations for FakeBash {
    fn exec<'a>(
        &'a self,
        _command: &'a str,
        _cwd: &'a Path,
        options: BashExecOptions,
    ) -> BoxFuture<'a, Result<Option<i64>, String>> {
        let output = self.output.clone();
        let outcome = self.outcome.clone();
        *self.env.lock().expect("env") = Some(options.env.clone());
        let on_data = options.on_data;
        Box::pin(async move {
            on_data(&output);
            outcome
        })
    }
}

fn bash_context(root: &Path, session: Option<(String, PathBuf)>) -> ToolContext {
    let mut context = ToolContext::new(root);
    if let Some((session_id, session_file)) = session {
        context.session_env = Some(Arc::new(ToolSessionEnv {
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: lazy_model(),
            thinking: ThinkingLevel::High,
            session_id: Some(session_id),
            session_file: Some(session_file),
            base_tools: Arc::new(ToolRegistry::new()),
            has_ui: false,
            project_trusted: true,
            user_agents_dir: None,
            agent_dir: None,
            credential: None,
            on_update: None,
            live_tools: None,
            active_override: None,
            background: None,
        }));
    }
    context
}

fn lazy_model() -> Model {
    // A minimal model is enough for the env variables; the tool never calls
    // the provider.
    Model {
        id: "fake-model".to_owned(),
        name: "Fake model".to_owned(),
        api: "fake-api".to_owned(),
        provider: "fake-provider".to_owned(),
        base_url: "http://localhost".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_owned()],
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

#[tokio::test]
async fn bash_with_operations_streams_and_injects_pi_session_env() {
    let root = temp_workspace("ops-bash");
    let session_file = root.join("session.jsonl");
    let operations = Arc::new(FakeBash::new(b"streamed\n", Ok(Some(0))));
    let tool = BashTool::with_operations(operations.clone());
    let context = bash_context(&root, Some(("session-42".to_owned(), session_file.clone())));
    let result = tool
        .run(&serde_json::json!({"command": "echo streamed"}), &context)
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "streamed\n");
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["exit_code"], 0);

    // PI_* session environment: every variable is cleared first, then the
    // session's values win (upstream `resolveSpawnContext`).
    let env = operations
        .env
        .lock()
        .expect("env")
        .clone()
        .expect("env passed to backend");
    assert_eq!(env["PI_SESSION_ID"].as_deref(), Some("session-42"));
    assert_eq!(
        env["PI_SESSION_FILE"].as_deref(),
        Some(session_file.to_string_lossy().as_ref())
    );
    assert_eq!(env["PI_PROVIDER"].as_deref(), Some("fake-provider"));
    assert_eq!(env["PI_MODEL"].as_deref(), Some("fake-model"));
    assert_eq!(env["PI_REASONING_LEVEL"].as_deref(), Some("high"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bash_local_child_sees_pi_session_environment() {
    let _agent_dir = crate::common::AgentDirEnvGuard::scratch("pi-session-local", None).await;
    if std::process::Command::new("bash")
        .arg("-c")
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        return;
    }
    let root = temp_workspace("pi-session-local");
    let session_file = root.join("session.jsonl");
    let tool = BashTool::new();
    let context = bash_context(
        &root,
        Some(("local-session".to_owned(), session_file.clone())),
    );
    let result = tool
        .run(
            &serde_json::json!({
                "command": "printf '%s|%s|%s|%s|%s' \"$PI_SESSION_ID\" \"$PI_SESSION_FILE\" \"$PI_PROVIDER\" \"$PI_MODEL\" \"$PI_REASONING_LEVEL\""
            }),
            &context,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        format!(
            "local-session|{}|fake-provider|fake-model|high",
            session_file.display()
        )
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bash_with_operations_maps_abort_and_timeout_errors() {
    let root = temp_workspace("ops-bash-errors");
    let aborted = BashTool::with_operations(Arc::new(FakeBash::new(
        b"partial\n",
        Err("aborted".to_owned()),
    )));
    let result = aborted
        .run(
            &serde_json::json!({"command": "sleep 10"}),
            &bash_context(&root, None),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("Command aborted"),
        "{}",
        result.content
    );

    let timed_out =
        BashTool::with_operations(Arc::new(FakeBash::new(b"", Err("timeout:5".to_owned()))));
    let result = timed_out
        .run(
            &serde_json::json!({"command": "sleep 10", "timeout": 5}),
            &bash_context(&root, None),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("Command timed out after 5 seconds"),
        "{}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn powershell_with_operations_uses_the_injected_backend() {
    let root = temp_workspace("ops-pwsh");
    let tool =
        PowerShellTool::with_operations(Arc::new(FakeBash::new(b"ps-output\n", Ok(Some(3)))));
    let result = tool
        .run(
            &serde_json::json!({"command": "Write-Output ps-output"}),
            &ToolContext::new(&root),
        )
        .await;
    assert!(result.is_error, "exit code 3 is an error result");
    assert!(
        result.content.contains("ps-output")
            && result.content.contains("Command exited with code 3"),
        "{}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

struct FakeLs {
    root_name: String,
    names: Vec<String>,
    dirs: Vec<String>,
}

impl LsOperations for FakeLs {
    fn exists<'a>(&'a self, _absolute_path: &'a Path) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn is_directory<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<bool>> {
        let root_name = self.root_name.clone();
        let dirs = self.dirs.clone();
        let path = absolute_path.to_path_buf();
        Box::pin(async move {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok(name == root_name || dirs.contains(&name))
        })
    }
    fn readdir<'a>(
        &'a self,
        _absolute_path: &'a Path,
    ) -> BoxFuture<'a, std::io::Result<Vec<String>>> {
        let names = self.names.clone();
        Box::pin(async move { Ok(names) })
    }
}

#[tokio::test]
async fn ls_with_operations_uses_the_injected_backend() {
    let root = temp_workspace("ops-ls");
    let tool = LsTool::with_operations(Arc::new(FakeLs {
        root_name: root
            .file_name()
            .expect("root name")
            .to_string_lossy()
            .into_owned(),
        names: vec!["b.txt".to_owned(), "a_dir".to_owned()],
        dirs: vec!["a_dir".to_owned()],
    }));
    let result = tool
        .run(&serde_json::json!({}), &ToolContext::new(&root))
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "a_dir/\nb.txt");
    let _ = std::fs::remove_dir_all(&root);
}

struct FakeFind {
    root: PathBuf,
}

impl FindOperations for FakeFind {
    fn exists<'a>(&'a self, _absolute_path: &'a Path) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn glob<'a>(
        &'a self,
        _pattern: &'a str,
        cwd: &'a Path,
        _options: FindGlobOptions,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        let absolute = cwd.join("abs.rs").to_string_lossy().into_owned();
        let _ = &self.root;
        Box::pin(async move { Ok(vec![absolute, "rel/nested.rs".to_owned()]) })
    }
}

#[tokio::test]
async fn find_with_operations_uses_the_injected_backend() {
    let root = temp_workspace("ops-find");
    let tool = FindTool::with_operations(Arc::new(FakeFind { root: root.clone() }));
    let result = tool
        .run(
            &serde_json::json!({"pattern": "*.rs"}),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "abs.rs\nrel/nested.rs");
    let _ = std::fs::remove_dir_all(&root);
}

struct FakeGrep;

impl GrepOperations for FakeGrep {
    fn is_directory<'a>(
        &'a self,
        _absolute_path: &'a Path,
    ) -> BoxFuture<'a, std::io::Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn read_file<'a>(&'a self, _absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<String>> {
        Box::pin(async { Ok("custom-context\nneedle\n".to_owned()) })
    }
}

#[tokio::test]
async fn grep_with_operations_reads_context_through_the_backend() {
    let root = temp_workspace("ops-grep");
    std::fs::write(root.join("m.txt"), "needle\n").expect("fixture");
    let tool = GrepTool::with_operations(Arc::new(FakeGrep));
    let result = tool
        .run(
            &serde_json::json!({"pattern": "needle", "path": "m.txt", "context": 1}),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("custom-context"),
        "context must come from the backend: {}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&root);
}
