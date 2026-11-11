//! Regression tests for the read-before-edit hard constraint:
//! `edit`/`write` must refuse (or warn about, in `PIR_READ_BEFORE_EDIT=warn`)
//! mutating an existing file that was not fully read in this session, or that
//! changed after the read. New-file writes stay allowed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::agent::read_ledger::{ReadBeforeEditMode, ReadLedger};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry, ToolSessionEnv};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::coding_agent::tools::{EditTool, ReadTool, WriteTool};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-read-before-edit-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).expect("write fixture");
        path
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).expect("read fixture")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "fake/fake-model".into(),
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

fn tool_context(cwd: &Path, ledger: Arc<ReadLedger>) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        signal: None,
        nested: None,
        session_env: Some(Arc::new(ToolSessionEnv {
            live_model: None,
            live_thinking: None,
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            thinking: ThinkingLevel::Off,
            session_id: Some("read-before-edit-test".into()),
            session_file: None,
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
            read_ledger: ledger,
        })),
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

async fn read(
    ctx: &ToolContext,
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> ToolResult {
    let mut arguments = serde_json::json!({ "path": path });
    if let Some(offset) = offset {
        arguments["offset"] = serde_json::json!(offset);
    }
    if let Some(limit) = limit {
        arguments["limit"] = serde_json::json!(limit);
    }
    ReadTool::new().run(&arguments, ctx).await
}

async fn edit(ctx: &ToolContext, path: &str, old: &str, new: &str) -> ToolResult {
    EditTool::new()
        .run(
            &serde_json::json!({
                "path": path,
                "edits": [{ "oldText": old, "newText": new }]
            }),
            ctx,
        )
        .await
}

async fn write(ctx: &ToolContext, path: &str, content: &str) -> ToolResult {
    WriteTool::new()
        .run(
            &serde_json::json!({ "path": path, "content": content }),
            ctx,
        )
        .await
}

fn assert_refused_mentioning(result: &ToolResult, needle: &str) {
    assert!(
        result.is_error,
        "expected a refusal, got: {}",
        result.content
    );
    assert!(
        result.content.contains(needle),
        "expected {needle:?} in: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_without_read_is_refused() {
    let dir = TempDir::new("unread");
    dir.write("note.txt", "alpha\nbeta\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let result = edit(&ctx, "note.txt", "beta", "BETA").await;
    assert_refused_mentioning(&result, "has not been read in this session");
    assert_eq!(dir.read("note.txt"), "alpha\nbeta\n");
}

#[tokio::test]
async fn edit_after_full_read_is_allowed() {
    let dir = TempDir::new("full-read");
    dir.write("note.txt", "alpha\nbeta\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let read_result = read(&ctx, "note.txt", None, None).await;
    assert!(
        !read_result.is_error,
        "read failed: {}",
        read_result.content
    );

    let result = edit(&ctx, "note.txt", "beta", "BETA").await;
    assert!(!result.is_error, "edit failed: {}", result.content);
    assert_eq!(dir.read("note.txt"), "alpha\nBETA\n");
}

#[tokio::test]
async fn edit_after_partial_read_is_refused() {
    let dir = TempDir::new("partial");
    dir.write("note.txt", "alpha\nbeta\ngamma\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let read_result = read(&ctx, "note.txt", Some(2), Some(1)).await;
    assert!(
        !read_result.is_error,
        "read failed: {}",
        read_result.content
    );
    assert!(
        read_result.content.contains("beta"),
        "{}",
        read_result.content
    );

    let result = edit(&ctx, "note.txt", "gamma", "GAMMA").await;
    assert_refused_mentioning(&result, "only part of the file has been read");
    assert_eq!(dir.read("note.txt"), "alpha\nbeta\ngamma\n");
}

#[tokio::test]
async fn edit_after_file_change_is_refused() {
    let dir = TempDir::new("stale");
    dir.write("note.txt", "alpha\nbeta\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let read_result = read(&ctx, "note.txt", None, None).await;
    assert!(
        !read_result.is_error,
        "read failed: {}",
        read_result.content
    );

    // Change both size and mtime; a size change also keeps the test stable on
    // filesystems with coarse timestamp resolution.
    dir.write("note.txt", "alpha\nbeta\ngamma\n");

    let result = edit(&ctx, "note.txt", "gamma", "GAMMA").await;
    assert_refused_mentioning(&result, "changed since it was last read");
    assert_eq!(dir.read("note.txt"), "alpha\nbeta\ngamma\n");
}

#[tokio::test]
async fn edit_after_own_edit_does_not_require_another_read() {
    let dir = TempDir::new("own-edit");
    dir.write("note.txt", "alpha\nbeta\ngamma\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let read_result = read(&ctx, "note.txt", None, None).await;
    assert!(
        !read_result.is_error,
        "read failed: {}",
        read_result.content
    );
    let first = edit(&ctx, "note.txt", "beta", "BETA").await;
    assert!(!first.is_error, "first edit failed: {}", first.content);
    let second = edit(&ctx, "note.txt", "gamma", "GAMMA").await;
    assert!(
        !second.is_error,
        "a follow-up edit after our own mutation must not require re-reading: {}",
        second.content
    );
    assert_eq!(dir.read("note.txt"), "alpha\nBETA\nGAMMA\n");
}

#[tokio::test]
async fn edit_warn_mode_allows_partial_read_but_still_blocks_write() {
    let dir = TempDir::new("edit-warn");
    dir.write("note.txt", "alpha\nbeta\ngamma\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::EditWarn)),
    );

    let read_result = read(&ctx, "note.txt", Some(2), Some(1)).await;
    assert!(
        !read_result.is_error,
        "read failed: {}",
        read_result.content
    );
    let edit_result = edit(&ctx, "note.txt", "gamma", "GAMMA").await;
    assert!(
        !edit_result.is_error,
        "edit should proceed with a warning: {}",
        edit_result.content
    );
    assert!(edit_result.content.contains("Warning:"));
    assert_eq!(dir.read("note.txt"), "alpha\nbeta\nGAMMA\n");

    let write_result = write(&ctx, "note.txt", "replacement\n").await;
    assert!(
        write_result.is_error,
        "write must remain strict in edit-warn mode: {}",
        write_result.content
    );
}

#[tokio::test]
async fn write_of_a_new_file_is_allowed_without_a_read() {
    let dir = TempDir::new("new-file");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let result = write(&ctx, "created.txt", "fresh\n").await;
    assert!(!result.is_error, "write failed: {}", result.content);
    assert_eq!(dir.read("created.txt"), "fresh\n");
}

#[tokio::test]
async fn edit_after_own_write_does_not_require_another_read() {
    let dir = TempDir::new("own-write");
    dir.write("note.txt", "alpha\nbeta\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Enforce)),
    );

    let read_result = read(&ctx, "note.txt", None, None).await;
    assert!(
        !read_result.is_error,
        "read failed: {}",
        read_result.content
    );
    let overwrite = write(&ctx, "note.txt", "one\ntwo\n").await;
    assert!(!overwrite.is_error, "write failed: {}", overwrite.content);
    let edit_result = edit(&ctx, "note.txt", "two", "TWO").await;
    assert!(
        !edit_result.is_error,
        "edit after our own write must not require re-reading: {}",
        edit_result.content
    );
    assert_eq!(dir.read("note.txt"), "one\nTWO\n");
}

#[tokio::test]
async fn off_mode_allows_mutation_without_read() {
    let dir = TempDir::new("off");
    dir.write("note.txt", "alpha\nbeta\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Off)),
    );

    let result = edit(&ctx, "note.txt", "beta", "BETA").await;
    assert!(!result.is_error, "edit failed: {}", result.content);
    assert!(!result.content.contains("Warning:"), "{}", result.content);
    assert_eq!(dir.read("note.txt"), "alpha\nBETA\n");

    let overwrite = write(&ctx, "note.txt", "overwritten\n").await;
    assert!(!overwrite.is_error, "write failed: {}", overwrite.content);
    assert_eq!(dir.read("note.txt"), "overwritten\n");
}

#[tokio::test]
async fn warn_mode_proceeds_and_reports_the_warning() {
    let dir = TempDir::new("warn");
    dir.write("note.txt", "alpha\nbeta\n");
    let ctx = tool_context(
        dir.path(),
        Arc::new(ReadLedger::new(ReadBeforeEditMode::Warn)),
    );

    let result = edit(&ctx, "note.txt", "beta", "BETA").await;
    assert!(!result.is_error, "edit failed: {}", result.content);
    assert!(
        result.content.contains("Warning:") && result.content.contains("PIR_READ_BEFORE_EDIT=warn"),
        "expected a warning, got: {}",
        result.content
    );
    assert_eq!(dir.read("note.txt"), "alpha\nBETA\n");
}
