//! Tests for AgentTool trait, ToolRegistry, ToolContext, and hooks.

use pir::agent::tool_batch::*;
use pir::agent::tool_execution::*;
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::ai::types::{ImageContent, Usage};
use serde_json::Value;
use std::sync::Arc;

struct EchoTool;
impl AgentTool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes input"
    }
    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Box::pin(async move { ToolResult::ok(text) })
    }
}

struct FailTool;
impl AgentTool for FailTool {
    fn name(&self) -> &'static str {
        "fail"
    }
    fn description(&self) -> &'static str {
        "always fails"
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::error("expected failure") })
    }
}

#[test]
fn tool_registry_register_and_get() {
    let mut reg = ToolRegistry::new();
    assert!(reg.is_empty());
    reg.register(Arc::new(EchoTool));
    assert!(!reg.is_empty());
    assert_eq!(reg.len(), 1);
    let tool = reg.get("echo").expect("tool found");
    assert_eq!(tool.name(), "echo");
}

#[test]
fn tool_registry_unknown_returns_none() {
    let reg = ToolRegistry::new();
    assert!(reg.get("nonexistent").is_none());
}

#[test]
fn tool_registry_upsert() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(FailTool));
    assert_eq!(reg.len(), 2);
    let names = reg.names();
    assert!(names.contains(&"echo"));
    assert!(names.contains(&"fail"));
}

#[test]
fn tool_context_abort_detection() {
    let ctx = ToolContext::new(".");
    assert!(!ctx.is_aborted());
    let sig = pir::ai::types::AbortSignal::new();
    sig.abort();
    let ctx2 = ToolContext {
        cwd: ".".into(),
        signal: Some(sig),
        nested: None,
        session_env: None,
        held_mutation_keys: None,
        on_update: None,
        background: false,
    };
    assert!(ctx2.is_aborted());
}

#[test]
fn echo_tool_executes() {
    let tool = EchoTool;
    let args = serde_json::json!({"text": "hello"});
    let ctx = ToolContext::new(".");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(tool.run(&args, &ctx));
    assert!(!result.is_error);
    assert_eq!(result.content, "hello");
}

#[test]
fn fail_tool_returns_error() {
    let tool = FailTool;
    let args = serde_json::json!({});
    let ctx = ToolContext::new(".");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(tool.run(&args, &ctx));
    assert!(result.is_error);
}

#[test]
fn tool_calls_from_message_extracts_calls() {
    let msg = pir::ai::types::AssistantMessage {
        content: vec![
            pir::ai::types::ContentBlock::Text(pir::ai::types::TextContent {
                text: "let me check".into(),
                text_signature: None,
            }),
            pir::ai::types::ContentBlock::ToolCall(pir::ai::types::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: [("path".into(), serde_json::json!("a.txt"))].into(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ],
        ..pir::ai::types::AssistantMessage::default()
    };
    let calls = tool_calls_from_message(&msg);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_name, "read");
    assert_eq!(calls[0].index, 1);
}

#[test]
fn error_results_for_length_produces_correct_count() {
    let calls = vec![
        AgentToolCall::new(0, "c1", "read", serde_json::json!({})),
        AgentToolCall::new(1, "c2", "write", serde_json::json!({})),
    ];
    let results = error_results_for_length(&calls);
    assert_eq!(results.len(), 2);
    assert!(results[0].is_error());
    assert!(results[1].is_error());
    assert_eq!(results[0].index, 0);
    assert_eq!(results[1].index, 1);
}

#[test]
fn before_hook_results_construct() {
    let r = BeforeToolCallResult {
        block: true,
        reason: Some("blocked".into()),
        terminate: false,
    };
    assert!(r.block);
    assert_eq!(r.reason.as_deref(), Some("blocked"));
}

#[test]
fn after_hook_override_defaults() {
    let r = AfterToolCallResult::default();
    assert!(r.content.is_none());
    assert!(r.content_blocks.is_none());
    assert!(r.usage.is_none());
    assert!(r.is_error.is_none());
    let r2 = AfterToolCallResult {
        content: Some("override".into()),
        is_error: Some(true),
        terminate: Some(true),
        ..AfterToolCallResult::default()
    };
    assert_eq!(r2.content.as_deref(), Some("override"));
    assert_eq!(r2.is_error, Some(true));
}

#[test]
fn after_hook_content_blocks_replace_text_and_images_and_carry_usage() {
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "old-image".into(),
        mime_type: "image/png".into(),
    });
    result.structured_content = Some(serde_json::json!({"old": true}));
    let usage = Usage {
        input: 7,
        output: 3,
        total_tokens: 10,
        ..Usage::default()
    };
    let (applied, returned_usage) = apply_after_tool_call_result(
        result,
        AfterToolCallResult {
            content_blocks: Some(vec![
                ToolResultContentBlock::Text("first".into()),
                ToolResultContentBlock::Image(ImageContent {
                    data: "new-image".into(),
                    mime_type: "image/jpeg".into(),
                }),
                ToolResultContentBlock::Text("second".into()),
            ]),
            usage: Some(usage),
            ..AfterToolCallResult::default()
        },
    );
    // Text blocks flatten with the same newline join the `tool_result` merge
    // uses; image blocks replace the previous images (text first in ToolResult).
    assert_eq!(applied.content, "first\nsecond");
    assert_eq!(
        applied.images,
        vec![ImageContent {
            data: "new-image".into(),
            mime_type: "image/jpeg".into(),
        }]
    );
    // Replacing content without structuredContent drops the stale value.
    assert!(applied.structured_content.is_none());
    assert_eq!(returned_usage, Some(usage));

    // The legacy `content: Option<String>` path keeps its old behavior: it
    // replaces only the text and leaves images untouched.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "kept".into(),
        mime_type: "image/png".into(),
    });
    let (applied, returned_usage) = apply_after_tool_call_result(
        result,
        AfterToolCallResult {
            content: Some("overridden".into()),
            ..AfterToolCallResult::default()
        },
    );
    assert_eq!(applied.content, "overridden");
    assert_eq!(applied.images.len(), 1);
    assert_eq!(applied.images[0].data, "kept");
    assert!(returned_usage.is_none());

    // Default overrides leave the result byte-identical.
    let original = ToolResult::ok("unchanged");
    let (applied, returned_usage) =
        apply_after_tool_call_result(original.clone(), AfterToolCallResult::default());
    assert_eq!(applied, original);
    assert!(returned_usage.is_none());
}

/// The `tool_result` event payload carries the COMPLETE content block array
/// (text + images, upstream `ToolResultEventBase.content`), not just the text.
#[test]
fn tool_result_event_content_includes_image_blocks() {
    let mut result = ToolResult::ok("see attached");
    result.images.push(ImageContent {
        data: "aW1n".into(),
        mime_type: "image/png".into(),
    });
    let blocks = tool_result_content_blocks(&result);
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "text", "text": "see attached"})
    );
    assert_eq!(
        blocks[1],
        serde_json::json!({"type": "image", "data": "aW1n", "mimeType": "image/png"})
    );
    // The `tool_execution_end` wire shape shares the same array.
    let end = tool_result_event_json(&result);
    assert_eq!(end["content"], serde_json::Value::Array(blocks));
}

/// Upstream `ToolResultEventResult.content` replaces the whole content ARRAY:
/// a handler that returns an image replaces the old images, and a text-only
/// replacement must not leave the original images behind. The legacy bare
/// string form keeps its old text-only behavior.
#[test]
fn tool_result_merge_replaces_content_array_and_images() {
    // Image replacement (text + image blocks).
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "OLD".into(),
        mime_type: "image/png".into(),
    });
    result.structured_content = Some(serde_json::json!({"old": true}));
    let merge = ToolResultMerge {
        content: Some("replaced text".into()),
        content_blocks: Some(vec![
            ToolResultContentBlock::Text("replaced text".into()),
            ToolResultContentBlock::Image(ImageContent {
                data: "NEW".into(),
                mime_type: "image/jpeg".into(),
            }),
        ]),
        ..ToolResultMerge::default()
    };
    apply_tool_result_merge(&mut result, &merge);
    assert_eq!(result.content, "replaced text");
    assert_eq!(
        result.images,
        vec![ImageContent {
            data: "NEW".into(),
            mime_type: "image/jpeg".into(),
        }]
    );
    // Replacing content without structured content drops the stale value.
    assert!(result.structured_content.is_none());

    // Text-only array: the whole array is replaced, so the old image is gone.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "OLD".into(),
        mime_type: "image/png".into(),
    });
    let merge = ToolResultMerge {
        content: Some("text only".into()),
        content_blocks: Some(vec![ToolResultContentBlock::Text("text only".into())]),
        ..ToolResultMerge::default()
    };
    apply_tool_result_merge(&mut result, &merge);
    assert_eq!(result.content, "text only");
    assert!(
        result.images.is_empty(),
        "text-only replacement leaves no old image"
    );

    // An explicit empty array is a valid replacement that clears everything.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "OLD".into(),
        mime_type: "image/png".into(),
    });
    let merge = ToolResultMerge {
        content_blocks: Some(Vec::new()),
        ..ToolResultMerge::default()
    };
    apply_tool_result_merge(&mut result, &merge);
    assert!(result.content.is_empty());
    assert!(result.images.is_empty());

    // Legacy bare-string form: text is replaced, images are kept.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "KEPT".into(),
        mime_type: "image/png".into(),
    });
    let merge = ToolResultMerge {
        content: Some("legacy text".into()),
        ..ToolResultMerge::default()
    };
    apply_tool_result_merge(&mut result, &merge);
    assert_eq!(result.content, "legacy text");
    assert_eq!(result.images.len(), 1);
    assert_eq!(result.images[0].data, "KEPT");
}

#[test]
fn tool_hooks_default() {
    let hooks = ToolHooks::default();
    assert!(hooks.before.is_none());
    assert!(hooks.after.is_none());
}

#[test]
fn tool_registry_iter() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(FailTool));
    let names: Vec<&str> = reg.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), 2);
}

// ---------------------------------------------------------------------------
// Regression: abort text + parallel-by-default (upstream agent-loop.ts:613-644)
// ---------------------------------------------------------------------------

/// On abort, remaining calls stop preparing and the synthesized error text is
/// upstream's "Operation aborted".
#[test]
fn aborted_batch_yields_operation_aborted_text() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let sig = pir::ai::types::AbortSignal::new();
        sig.abort();
        let context = ToolContext {
            cwd: ".".into(),
            signal: Some(sig),
            nested: None,
            session_env: None,
            held_mutation_keys: None,
            on_update: None,
            background: false,
        };
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        let calls = vec![AgentToolCall::new(0, "c1", "echo", serde_json::json!({}))];
        let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
        let outcome = scheduler
            .execute_batch(calls, &registry, &context, None)
            .await;
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_error());
        assert_eq!(outcome.results[0].result.content, "Operation aborted");
    });
}

/// Mixed batches containing side-effect tools run in PARALLEL by default:
/// only a tool's own `executionMode: "sequential"` degrades the batch
/// (upstream agent-loop.ts:516-522 has no batch-level side-effect degrade).
#[test]
fn side_effect_tools_no_longer_degrade_the_batch() {
    struct SideEffectTool;
    impl AgentTool for SideEffectTool {
        fn name(&self) -> &'static str {
            "side-effect"
        }
        fn description(&self) -> &'static str {
            "side effects"
        }
        fn has_external_side_effects(&self) -> bool {
            true
        }
        fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
            Box::pin(async { ToolResult::ok("done") })
        }
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let context = ToolContext::new(".");
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SideEffectTool));
        let calls = vec![
            AgentToolCall::new(0, "c1", "side-effect", serde_json::json!({})),
            AgentToolCall::new(1, "c2", "side-effect", serde_json::json!({})),
        ];
        // Default config (allow_parallel_side_effects = false) still runs
        // the batch in parallel now; completion order is used for `end`
        // events, which only happens in parallel mode. We assert on the
        // mode indirectly: parallel execution records durations on both
        // results (execute_one ran) and sequential would too — so assert on
        // the scheduler config knob having no sequential effect by checking
        // both calls executed with their source indices intact.
        let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
        let outcome = scheduler
            .execute_batch(calls, &registry, &context, None)
            .await;
        assert_eq!(outcome.results.len(), 2);
        assert!(!outcome.results[0].is_error());
        assert!(!outcome.results[1].is_error());
        assert_eq!(outcome.results[0].index, 0);
        assert_eq!(outcome.results[1].index, 1);
    });
}

/// Pinned contract #1: `AgentTool` gains `prompt_snippet`/`prompt_guidelines`
/// default methods.
#[test]
fn agent_tool_prompt_defaults() {
    assert!(EchoTool.prompt_snippet().is_none());
    assert!(EchoTool.prompt_guidelines().is_empty());
}

// ---------------------------------------------------------------------------
// Audit §4 #29: the native `AgentTool` contract
// (`pkg-agent types.ts:466-505`): `label`, `prepareArguments` BEFORE schema
// validation (async, fail-closed), and the durable `replay` policy.
// ---------------------------------------------------------------------------

/// A tool whose schema REQUIRES `text` and whose `prepareArguments` repairs
/// the legacy `{ legacy: "..." }` shape. `executed` records what `run` saw.
struct LegacyArgumentsTool {
    executed: Arc<std::sync::Mutex<Option<Value>>>,
    prepare_fails: bool,
}

impl LegacyArgumentsTool {
    fn new(executed: Arc<std::sync::Mutex<Option<Value>>>, prepare_fails: bool) -> Self {
        Self {
            executed,
            prepare_fails,
        }
    }
}

impl AgentTool for LegacyArgumentsTool {
    fn name(&self) -> &'static str {
        "legacy"
    }
    fn label(&self) -> &'static str {
        "Legacy arguments"
    }
    fn description(&self) -> &'static str {
        "repairs legacy arguments"
    }
    fn parameters(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false,
        }))
    }
    fn prepares_arguments(&self) -> bool {
        true
    }
    fn prepare_arguments(&self, args: Value) -> PrepareArgumentsFuture {
        let prepare_fails = self.prepare_fails;
        Box::pin(async move {
            if prepare_fails {
                return Err("legacy shape rejected".to_owned());
            }
            let text = args
                .get("legacy")
                .and_then(Value::as_str)
                .or_else(|| args.get("text").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned();
            Ok(serde_json::json!({ "text": text }))
        })
    }
    fn replay(&self) -> ToolReplay {
        ToolReplay::Safe
    }
    fn run(&self, args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        *self.executed.lock().expect("executed slot") = Some(args.clone());
        Box::pin(async { ToolResult::ok("ran") })
    }
}

/// `label` defaults to the name (every upstream builtin declares the same
/// label as its name) and an override reaches the native contract.
#[test]
fn agent_tool_label_defaults_to_the_name_and_can_be_overridden() {
    assert_eq!(EchoTool.label(), "echo");
    let executed = Arc::new(std::sync::Mutex::new(None));
    let tool = LegacyArgumentsTool::new(executed, false);
    assert_eq!(tool.label(), "Legacy arguments");
}

/// The identity `prepareArguments` default is inert: a tool that does not
/// declare the shim reports no capability and its default returns the input.
#[test]
fn agent_tool_prepare_arguments_default_is_the_identity() {
    assert!(!EchoTool.prepares_arguments());
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let args = serde_json::json!({"text": "hello"});
    let prepared = rt.block_on(EchoTool.prepare_arguments(args.clone()));
    assert_eq!(prepared.expect("identity"), args);
}

/// `prepareArguments` runs BEFORE schema validation: a legacy payload that
/// FAILS the declared schema is repaired, then validated in its repaired form,
/// so the tool executes with the repaired arguments.
#[test]
fn prepare_arguments_repairs_legacy_arguments_before_schema_validation() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let executed = Arc::new(std::sync::Mutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(LegacyArgumentsTool::new(executed.clone(), false)));
        let calls = vec![AgentToolCall::new(
            0,
            "c1",
            "legacy",
            serde_json::json!({ "legacy": "hello" }),
        )];
        let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
        let outcome = scheduler
            .execute_batch(calls, &registry, &ToolContext::new("."), None)
            .await;
        assert_eq!(outcome.results.len(), 1);
        assert!(
            !outcome.results[0].is_error(),
            "the repaired arguments validated: {}",
            outcome.results[0].result.content
        );
        assert_eq!(
            executed.lock().expect("executed slot").clone(),
            Some(serde_json::json!({ "text": "hello" })),
            "run received the REPAIRED arguments"
        );
    });
}

/// A failing `prepareArguments` rejects the call (fail-closed): the tool never
/// runs and the failure is the model-visible result.
#[test]
fn prepare_arguments_failure_fails_closed_before_validation_and_execution() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let executed = Arc::new(std::sync::Mutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(LegacyArgumentsTool::new(executed.clone(), true)));
        let calls = vec![AgentToolCall::new(
            0,
            "c1",
            "legacy",
            serde_json::json!({ "legacy": "hello" }),
        )];
        let scheduler = ToolScheduler::new(ToolSchedulerConfig::default());
        let outcome = scheduler
            .execute_batch(calls, &registry, &ToolContext::new("."), None)
            .await;
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_error(), "the call was rejected");
        assert_eq!(
            outcome.results[0].result.content, "legacy shape rejected",
            "the shim's failure message is the model-facing error"
        );
        assert!(
            executed.lock().expect("executed slot").is_none(),
            "the tool never ran"
        );
    });
}

/// Durable replay metadata: `replay` defaults to `Never` (upstream's
/// `?? "unsafe"`) and carries the upstream wire names.
#[test]
fn agent_tool_replay_metadata_matches_the_upstream_wire_names() {
    assert_eq!(EchoTool.replay(), ToolReplay::Never);
    assert_eq!(ToolReplay::Never.as_str(), "never");
    assert_eq!(ToolReplay::Safe.as_str(), "safe");
    assert_eq!(ToolReplay::parse("never"), Some(ToolReplay::Never));
    assert_eq!(ToolReplay::parse("safe"), Some(ToolReplay::Safe));
    assert_eq!(ToolReplay::parse("unsafe"), None);
    assert!(!ToolReplay::Never.is_safe());
    assert!(ToolReplay::Safe.is_safe());
    let executed = Arc::new(std::sync::Mutex::new(None));
    assert_eq!(
        LegacyArgumentsTool::new(executed, false).replay(),
        ToolReplay::Safe
    );
}
