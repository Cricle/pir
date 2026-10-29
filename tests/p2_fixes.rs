//! Regression tests for two independent P2 fixes:
//!
//! - **P2-c** (`src/plugins/subagent.rs`): an explicit `provider/id` model
//!   spec must resolve strictly within that provider — no cross-provider
//!   fallback to a same-id model of a different provider. A bare single-segment
//!   id still does the global search.
//! - **P2-f** (`src/tui/theme.rs`): parse-level guarantees the late-reply
//!   handling relies on: an OSC 11 reply that arrives after the query window
//!   still parses into a background color, and ordinary keystroke noise does
//!   NOT parse (so it is never mistaken for a reply).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry, ToolSessionEnv};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::plugins::subagent::SubagentTool;
use pir::tui::theme::{appearance_from_background, parse_osc11_background, Appearance};

// ---------------------------------------------------------------------------
// Fakes (same pattern as tests/subagent_builtin.rs)
// ---------------------------------------------------------------------------

fn model_with(provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: format!("{provider}/{id}"),
        api: "fake-api".into(),
        provider: provider.into(),
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        usage: Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.25,
                ..Cost::default()
            },
            ..Usage::default()
        },
        timestamp: 1,
        ..Default::default()
    }
}

fn user_text(context: &Context) -> String {
    context
        .messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::User(user) => user.content.iter().find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .next()
        .unwrap_or_default()
}

fn has_tool_result(context: &Context) -> bool {
    context
        .messages
        .last()
        .is_some_and(|message| matches!(message, Message::ToolResult(_)))
}

fn tool_call_block(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider with a configurable id and model list, so two providers
/// can expose same-id (or disjoint) models.
struct NamedProvider {
    id: &'static str,
    models: Vec<Model>,
    respond: Responder,
}

impl Provider for NamedProvider {
    fn id(&self) -> &str {
        self.id
    }
    fn name(&self) -> &str {
        self.id
    }
    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let message = (self.respond)(0, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn registry_with(
    providers: &[(&'static str, &[&str])],
    respond: Responder,
) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    for (id, model_ids) in providers {
        registry.set_provider(Arc::new(NamedProvider {
            id,
            models: model_ids.iter().map(|mid| model_with(id, mid)).collect(),
            respond: Arc::clone(&respond),
        }));
    }
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-p2-fixes-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write_agent(&self, name: &str, frontmatter: &str, body: &str) {
        let dir = self.0.join("agents");
        std::fs::create_dir_all(&dir).expect("create agents dir");
        std::fs::write(dir.join(name), format!("---\n{frontmatter}---\n{body}"))
            .expect("write agent");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tool_context(cwd: &Path, registry: Arc<ProviderRegistry>) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        signal: None,
        nested: None,
        session_env: Some(Arc::new(ToolSessionEnv {
            provider_registry: registry,
            model: model_with("fake", "fake-model"),
            thinking: ThinkingLevel::Off,
            session_id: None,
            session_file: None,
            base_tools: Arc::new(ToolRegistry::new()),
            has_ui: false,
            project_trusted: true,
            user_agents_dir: Some(cwd.join("agents")),
            agent_dir: None,
            credential: None,
            on_update: None,
            live_tools: None,
            active_override: None,
            background: None,
        })),
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

async fn run_tool(
    args: serde_json::Value,
    ctx: &ToolContext,
) -> pir::agent::tool_batch::ToolResult {
    SubagentTool::default().run(&args, ctx).await
}

/// The standard two-turn flow: the parent dispatches the subagent, the child
/// answers its task.
fn standard_responder() -> Responder {
    Arc::new(|_, context| {
        if has_tool_result(context) {
            assistant(vec![text("parent done")], StopReason::Stop)
        } else if user_text(context) == "the task" {
            assistant(vec![text("CHILD_RESULT")], StopReason::Stop)
        } else {
            assistant(
                vec![tool_call_block(
                    "call-1",
                    "subagent",
                    serde_json::json!({ "agent": "scout", "task": "the task" }),
                )],
                StopReason::ToolUse,
            )
        }
    })
}

// ---------------------------------------------------------------------------
// P2-c: `provider/id` resolves strictly within that provider
// ---------------------------------------------------------------------------

const TWO_PROVIDERS: &[(&str, &[&str])] = &[("aaa", &["aaa-only"]), ("bbb", &["bbb-only"])];

#[tokio::test]
async fn provider_id_spec_resolves_within_that_provider() {
    let temp = TempDir::new("p2c-exact");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: d\nmodel: aaa/aaa-only\n",
        "b",
    );
    let registry = registry_with(TWO_PROVIDERS, standard_responder());
    let ctx = tool_context(&temp.0, registry);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "the task" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "CHILD_RESULT");
}

#[tokio::test]
async fn provider_id_spec_does_not_fall_back_to_another_provider() {
    // "bbb/aaa-only" names a model that only provider "aaa" has. The old
    // cross-provider fallback silently routed the child to aaa's model; now
    // it must fail with an unknown-model error.
    let temp = TempDir::new("p2c-strict");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: d\nmodel: bbb/aaa-only\n",
        "b",
    );
    let registry = registry_with(TWO_PROVIDERS, standard_responder());
    let ctx = tool_context(&temp.0, registry);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "the task" }),
        &ctx,
    )
    .await;
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("Unknown model: \"bbb/aaa-only\""),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn bare_id_spec_still_matches_globally() {
    // A bare single-segment id keeps the global (all-provider) search.
    let temp = TempDir::new("p2c-bare");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: d\nmodel: aaa-only\n",
        "b",
    );
    let registry = registry_with(TWO_PROVIDERS, standard_responder());
    let ctx = tool_context(&temp.0, registry);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "the task" }),
        &ctx,
    )
    .await;
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(result.content, "CHILD_RESULT");
}

#[tokio::test]
async fn unknown_provider_in_spec_fails() {
    let temp = TempDir::new("p2c-ghost-provider");
    temp.write_agent(
        "scout.md",
        "name: scout\ndescription: d\nmodel: ghost/aaa-only\n",
        "b",
    );
    let registry = registry_with(&[("aaa", &["aaa-only"])], standard_responder());
    let ctx = tool_context(&temp.0, registry);
    let result = run_tool(
        serde_json::json!({ "agent": "scout", "task": "the task" }),
        &ctx,
    )
    .await;
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("Unknown model: \"ghost/aaa-only\""),
        "content: {}",
        result.content
    );
}

// ---------------------------------------------------------------------------
// P2-f: late OSC 11 reply parse guarantees
// ---------------------------------------------------------------------------

#[test]
fn late_osc11_reply_forms_still_parse() {
    // BEL-terminated reply (the common shape).
    assert_eq!(
        parse_osc11_background("\x1b]11;rgb:1a2b/3c4d/5e6f\x07"),
        Some((0x1a, 0x3c, 0x5e))
    );
    // ST-terminated reply.
    assert_eq!(
        parse_osc11_background("\x1b]11;#102030\x1b\\"),
        Some((0x10, 0x20, 0x30))
    );
    // 12-digit hex reply (some terminals report 16-bit channels).
    assert_eq!(
        parse_osc11_background("\x1b]11;#0000ffff8000\x07"),
        Some((0x00, 0xff, 0x80))
    );
}

#[test]
fn keystroke_noise_never_parses_as_a_reply() {
    // Ordinary typed bytes and a truncated reply must NOT be classified as a
    // background reply (they take the stashed-and-dropped path instead of
    // being misapplied as a theme change).
    assert_eq!(parse_osc11_background("abc"), None);
    assert_eq!(parse_osc11_background("\x03"), None); // Ctrl+C byte
    assert_eq!(parse_osc11_background("\x1b]11;#1020"), None); // partial
    assert_eq!(parse_osc11_background(""), None);
}

#[test]
fn appearance_classification_for_late_replies() {
    // Dark gray: white text has more contrast on it than black → dark
    // (upstream terminalAppearance: whiteContrast >= blackContrast → dark).
    assert_eq!(appearance_from_background((64, 64, 64)), Appearance::Dark);
    // Near white → light.
    assert_eq!(
        appearance_from_background((250, 250, 250)),
        Appearance::Light
    );
    // Black → dark.
    assert_eq!(appearance_from_background((0, 0, 0)), Appearance::Dark);
}
