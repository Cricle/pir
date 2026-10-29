//! Native `RendererService` / `CompletionService` binding and TUI resolution.
//!
//! These tests build a real `AgentSession` with one inline probe plugin (no JS
//! runtime, no on-disk configuration), capture the bound `PluginContext`, and
//! exercise the two live services through the same TUI-facing helpers the TUI
//! uses:
//!
//! - `tui::renderers::{native_message_lines, native_tool_lines}` — the native
//!   first/after-JS resolver seams;
//! - `tui::autocomplete::{merge_suggestion_items, completion_response_is_current}`
//!   — the native-after-JS merge and the stale-response drop.
//!
//! The service registries are host-scoped, so each test's fresh session owns
//! isolated state.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value as JsonValue};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::extensions::RendererOutcome;
use pir::plugins::plugin::services::{
    ArgumentCompletions, AutocompleteItem as ServiceItem, AutocompleteProvider, CompletionContext,
    MarkdownTransformer, MessageRenderer, RendererKind, ServiceFuture, ToolRenderRequest,
    ToolRendererResolver,
};
use pir::plugins::plugin::PluginContext;
use pir::plugins::{AgentPlugin, PluginInfo};
use pir::tui::autocomplete::{
    autocomplete_item_from_service, completion_response_is_current, merge_suggestion_items,
    AutocompleteItem as TuiItem,
};
use pir::tui::renderers::{native_message_lines, native_tool_lines};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Captures the `PluginContext` the live session binds.
struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
}

impl ProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn last_context(&self) -> PluginContext {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .expect("the session bound a plugin context")
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ctx.clone());
    }
}

fn fake_model() -> Model {
    Model {
        id: "model-a".to_owned(),
        name: "Model A".to_owned(),
        provider: "fake".to_owned(),
        api: "fake".to_owned(),
        base_url: String::new(),
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

/// Build a hermetic session with one inline plugin; no session storage and no
/// extension runtime are needed to exercise the service binding.
fn build_session(plugin: Arc<ProbePlugin>) -> Arc<AgentSession> {
    AgentSession::new_arc_with_plugins(
        AgentSessionConfig {
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![plugin as Arc<dyn AgentPlugin>],
    )
}

struct LinesRenderer(Vec<String>);

impl MessageRenderer for LinesRenderer {
    fn render(&self, _message: &JsonValue, _width: usize) -> RendererOutcome {
        RendererOutcome {
            lines: Some(self.0.clone()),
            text: None,
        }
    }
}

struct PanicRenderer;

impl MessageRenderer for PanicRenderer {
    fn render(&self, _message: &JsonValue, _width: usize) -> RendererOutcome {
        panic!("native renderer boom")
    }
}

/// Appends its suffix, so a sequence of transformers proves the composition
/// order.
struct SuffixTransformer(&'static str);

impl MarkdownTransformer for SuffixTransformer {
    fn transform(&self, markdown: &str) -> Option<String> {
        Some(format!("{markdown}{}", self.0))
    }
}

struct NativeToolResolver;

impl ToolRendererResolver for NativeToolResolver {
    fn render(&self, request: &ToolRenderRequest<'_>) -> RendererOutcome {
        RendererOutcome {
            lines: Some(vec![format!("native:{}", request.tool_name)]),
            text: None,
        }
    }
}

struct ItemsProvider(Vec<ServiceItem>);

impl AutocompleteProvider for ItemsProvider {
    fn suggest(&self, _context: &CompletionContext) -> ServiceFuture<'static, Vec<ServiceItem>> {
        let items = self.0.clone();
        Box::pin(async move { items })
    }
}

struct PanicProvider;

impl AutocompleteProvider for PanicProvider {
    fn suggest(&self, _context: &CompletionContext) -> ServiceFuture<'static, Vec<ServiceItem>> {
        Box::pin(async { panic!("native completion provider boom") })
    }
}

struct ItemsArguments(Vec<ServiceItem>);

impl ArgumentCompletions for ItemsArguments {
    fn complete(&self, _prefix: &str) -> ServiceFuture<'static, Vec<ServiceItem>> {
        let items = self.0.clone();
        Box::pin(async move { items })
    }
}

struct PanicArguments;

impl ArgumentCompletions for PanicArguments {
    fn complete(&self, _prefix: &str) -> ServiceFuture<'static, Vec<ServiceItem>> {
        Box::pin(async { panic!("native argument provider boom") })
    }
}

fn probe_context(plugin: &Arc<ProbePlugin>) -> PluginContext {
    plugin.last_context()
}

// ---------------------------------------------------------------------------
// RendererService
// ---------------------------------------------------------------------------

#[test]
fn native_message_renderer_resolves_and_unknown_type_falls_back() {
    let plugin = ProbePlugin::new("renderer-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let renderers = ctx.renderers().expect("renderer service bound");

    renderers.register_message(
        "probe",
        Arc::new(LinesRenderer(vec!["probe: custom".to_owned()])),
    );

    assert!(
        renderers.has(RendererKind::Message, "probe"),
        "has(Message, probe) reflects the registration"
    );
    assert_eq!(
        native_message_lines(
            renderers,
            "probe",
            &json!({ "customType": "probe", "content": "hi" }),
            80
        ),
        Some(vec!["probe: custom".to_owned()])
    );

    // An unregistered custom type is "not handled": the caller keeps the JS
    // and built-in paths.
    assert!(!renderers.has(RendererKind::Message, "other"));
    assert_eq!(
        native_message_lines(renderers, "other", &json!({ "customType": "other" }), 80),
        None
    );

    drop(session);
}

#[test]
fn native_markdown_transformers_compose_in_registration_order_after_js() {
    let plugin = ProbePlugin::new("markdown-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let renderers = ctx.renderers().expect("renderer service bound");

    renderers.register_markdown(Arc::new(SuffixTransformer("-one")));
    renderers.register_markdown(Arc::new(SuffixTransformer("-two")));
    assert_eq!(renderers.markdown_transformer_count(), 2);
    assert!(renderers.has(RendererKind::Markdown, ""));
    assert_eq!(
        renderers.transform_markdown("text"),
        Some("text-one-two".to_owned()),
        "native transformers run in registration order"
    );

    // The TUI runs the JS chain first and feeds its output to the native
    // chain; simulate the JS result at the same seam.
    let after_js = "text-js";
    assert_eq!(
        renderers.transform_markdown(after_js),
        Some("text-js-one-two".to_owned()),
        "native transformers compose AFTER the JS chain"
    );

    drop(session);
}

#[test]
fn panicking_native_renderer_is_isolated_and_falls_back() {
    let plugin = ProbePlugin::new("panic-renderer-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let renderers = ctx.renderers().expect("renderer service bound");

    renderers.register_message("boom", Arc::new(PanicRenderer));
    assert!(renderers.has(RendererKind::Message, "boom"));
    // The panic is caught inside the service: the outcome is "unhandled", so
    // the TUI falls back to JS/built-in instead of unwinding.
    assert_eq!(
        native_message_lines(renderers, "boom", &json!({ "customType": "boom" }), 80),
        None
    );

    drop(session);
}

#[test]
fn native_tool_resolver_chain_is_consulted() {
    let plugin = ProbePlugin::new("tool-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let renderers = ctx.renderers().expect("renderer service bound");

    renderers.register_tool(Some("probe-tool"), Arc::new(NativeToolResolver));
    assert!(renderers.has(RendererKind::Tool, "probe-tool"));
    let args = json!({ "path": "x" });
    let result = json!({ "output": "ok" });
    let request = ToolRenderRequest {
        tool_name: "probe-tool",
        args: &args,
        result: &result,
        is_error: false,
        width: 80,
    };
    assert_eq!(
        native_tool_lines(renderers, &request),
        Some(vec!["native:probe-tool".to_owned()])
    );

    drop(session);
}

// ---------------------------------------------------------------------------
// CompletionService
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn native_suggestions_merge_once_and_stale_responses_are_dropped() {
    let plugin = ProbePlugin::new("completion-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let completions = ctx.completions().expect("completion service bound");

    completions.register_provider(Arc::new(ItemsProvider(vec![
        ServiceItem::new("foo", "foo"),
        ServiceItem::new("foo", "duplicate"),
    ])));
    completions.register_provider(Arc::new(ItemsProvider(vec![
        ServiceItem::new("foo", "duplicate-2"),
        ServiceItem::new("fob", "fob"),
    ])));
    assert!(completions.has_providers());

    let context = CompletionContext {
        prefix: "fo".to_owned(),
        line: "fo".to_owned(),
        cursor: 2,
    };
    let native = completions.suggest(&context).await;
    assert_eq!(
        native
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        vec!["foo", "fob"],
        "provider order is preserved and values are deduped"
    );

    // The dispatcher merges the JS chain first, the native chain after it.
    let merged = merge_suggestion_items(
        vec![TuiItem::new("foo", "foo-js")],
        native
            .into_iter()
            .map(autocomplete_item_from_service)
            .collect(),
    );
    assert_eq!(merged.len(), 2, "foo appears exactly once after the merge");
    assert_eq!(merged[0].label, "foo-js", "the JS row wins the duplicate");
    assert_eq!(merged[1].value, "fob");

    // Prefix dedupe / race handling: the reply is accepted while the editor
    // still holds the requested prefix and dropped once it moved on.
    assert!(completion_response_is_current("fo", "fo"));
    assert!(!completion_response_is_current("foo", "fo"));

    drop(session);
}

#[tokio::test(flavor = "current_thread")]
async fn native_argument_provider_merges_after_js() {
    let plugin = ProbePlugin::new("argument-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let completions = ctx.completions().expect("completion service bound");

    completions.register_argument(
        "deploy",
        Arc::new(ItemsArguments(vec![ServiceItem::new("prod", "prod")])),
    );
    assert!(completions.has_argument_provider("deploy"));
    assert!(!completions.has_argument_provider("unknown"));

    let native = completions.argument_completions("deploy", "pr").await;
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].value, "prod");

    // Simulated JS `getArgumentCompletions` rows merge first; the native row
    // is appended only when its value is new.
    let merged = merge_suggestion_items(
        vec![
            TuiItem::new("prod", "prod-js"),
            TuiItem::new("staging", "staging"),
        ],
        native
            .into_iter()
            .map(autocomplete_item_from_service)
            .collect(),
    );
    assert_eq!(
        merged
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        vec!["prod", "staging"]
    );
    assert_eq!(merged[0].label, "prod-js", "JS keeps precedence");
    assert_eq!(
        completions.argument_completions("unknown", "").await,
        Vec::new()
    );

    drop(session);
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_providers_are_ignored_and_others_still_run() {
    let plugin = ProbePlugin::new("completion-error-probe");
    let session = build_session(Arc::clone(&plugin));
    let ctx = probe_context(&plugin);
    let completions = ctx.completions().expect("completion service bound");

    completions.register_provider(Arc::new(PanicProvider));
    completions.register_provider(Arc::new(ItemsProvider(vec![ServiceItem::new(
        "still-here",
        "still-here",
    )])));
    completions.register_argument("boom", Arc::new(PanicArguments));

    let context = CompletionContext {
        prefix: "st".to_owned(),
        line: "st".to_owned(),
        cursor: 2,
    };
    let items = completions.suggest(&context).await;
    assert_eq!(
        items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        vec!["still-here"],
        "a panicking provider contributes nothing and does not stop the next one"
    );
    assert_eq!(
        completions.argument_completions("boom", "").await,
        Vec::new()
    );

    drop(session);
}
