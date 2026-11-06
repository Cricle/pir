//! P0-1: Unicode-aware token estimation (CJK fairness) across the shared
//! compaction trigger, chunking, and `context_usage` paths.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::compaction;
use pir::compaction::paged::{PagedCompactionInputs, PagedRequestPrompt};
use pir::compaction::CompleteFn;
use pir::session_manager::SessionManager;

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage::default(),
        timestamp: 2,
        ..Default::default()
    })
}

fn fake_model(context_window: u64) -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
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
        context_window,
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

fn text_tokens(text: &str) -> u64 {
    compaction::estimate_text_tokens(text)
}

#[test]
fn cjk_estimates_are_fair_and_never_below_half_the_character_count() {
    let cjk_only = [
        ("中文", "这是一个用于测试 token 估算的中文句子。"),
        ("日本語", "これは日本語のトークン推定を確認する文章です。"),
        ("한국어", "이것은 한국어 토큰 추정을 확인하는 문장입니다."),
    ];
    for (label, text) in cjk_only {
        let chars = text.chars().count() as u64;
        let tokens = text_tokens(text);
        assert!(
            tokens > 0,
            "{label}: non-empty text must estimate > 0 tokens"
        );
        assert!(
            tokens >= chars / 2,
            "{label}: {tokens} tokens for {chars} chars is below the 0.5/char floor"
        );
    }
    // Mixed text: the CJK run alone must still be charged at ~1 token/char
    // even when ASCII surrounds it.
    let mixed = "Hello 世界! こんにちは 12345 안녕하세요 mixed-code fn main() {}";
    let cjk_chars = mixed
        .chars()
        .filter(|ch| {
            matches!(
                *ch as u32,
                0x3040..=0x30FF | 0x3400..=0x9FFF | 0xAC00..=0xD7AF
            )
        })
        .count() as u64;
    assert!(
        text_tokens(mixed) >= cjk_chars,
        "mixed: {} tokens must cover {cjk_chars} CJK chars",
        text_tokens(mixed)
    );
    let code = "fn main() { let x = compute(1, 2, 3); println!(\"{x}\"); }";
    let code_chars = code.chars().count() as u64;
    assert!(
        text_tokens(code) <= code_chars,
        "ASCII code must stay near chars/4, got {} for {code_chars}",
        text_tokens(code)
    );
    let emoji = "😀😀😀🚀🎉";
    assert!(
        text_tokens(emoji) >= emoji.chars().count() as u64,
        "emoji must be weighted above one token per character"
    );
}

#[test]
fn cjk_one_token_per_character_ascii_four_chars_per_token() {
    let han = "漢字".repeat(100); // 200 chars
    assert_eq!(text_tokens(&han), 200);
    let ascii = "a".repeat(400);
    assert_eq!(text_tokens(&ascii), 100);
    let emoji = "😀".repeat(10);
    assert_eq!(text_tokens(&emoji), 20);
}

#[test]
fn conservative_estimate_is_never_lower_than_normal() {
    for text in [
        "plain ascii sentence",
        "中文句子",
        "こんにちは世界",
        "😀🚀",
        "mixed 中文 and ascii",
    ] {
        let normal = compaction::estimate_text_tokens(text);
        let conservative = compaction::estimate_text_tokens_conservative(text);
        assert!(
            conservative >= normal,
            "{text:?}: conservative {conservative} < normal {normal}"
        );
    }
}

#[test]
fn compaction_trigger_counts_cjk_tokens_not_ascii_chars() {
    // 20k Han characters are ~20k tokens, but only ~5k under chars/4. With a
    // 16k window and a 4k reserve, the old estimator stayed far below the
    // trigger; the shared estimator must trigger.
    let messages = vec![user_message(&"漢".repeat(20_000))];
    let context = Context {
        system_prompt: None,
        messages,
        tools: None,
    };
    let model = fake_model(16_384);
    let config = compaction::CompactionConfig {
        enabled: true,
        reserve_tokens: 4_096,
        keep_recent_tokens: 2_000,
    };
    assert!(
        compaction::should_compact(&context, &model, &config),
        "20k CJK chars must exceed a 12,288-token usable window"
    );

    let short = Context {
        system_prompt: None,
        messages: vec![user_message(&"a".repeat(20_000))],
        tools: None,
    };
    assert!(
        !compaction::should_compact(&short, &fake_model(16_384), &config),
        "20k ASCII chars are ~5k tokens and must not trigger"
    );
}

#[test]
fn request_prompt_estimate_uses_the_same_cjk_weights() {
    let prompt = "系統提示".repeat(100); // 400 Han chars ~400 tokens
    let tools = vec![Tool {
        name: "echo".into(),
        description: "返回输入".into(),
        parameters: serde_json::json!({"type": "object"}),
        constrained_sampling: None,
    }];
    let tokens = compaction::estimate_prompt_tokens(Some(&prompt), Some(&tools));
    assert!(
        tokens >= 400,
        "the CJK system prompt alone must dominate: {tokens}"
    );
}

fn threshold_config() -> compaction::CompactionConfig {
    compaction::CompactionConfig {
        enabled: true,
        reserve_tokens: 4_096,
        keep_recent_tokens: 2_000,
    }
}

#[test]
fn compaction_trigger_counts_large_system_prompt_when_no_usage() {
    // Messages alone are ~1,000 tokens (far below the 12,288-token usable
    // window); the 60,000-char prompt adds ~15,000 tokens, so only a
    // full-request estimate may trigger.
    let messages = vec![user_message(&"a".repeat(4_000))];
    let model = fake_model(16_384);
    let config = threshold_config();
    let without_prompt = Context {
        system_prompt: None,
        messages: messages.clone(),
        tools: None,
    };
    assert_eq!(
        compaction::estimate_context_tokens(&without_prompt.messages).tokens,
        1_000
    );
    assert!(!compaction::should_compact(
        &without_prompt,
        &model,
        &config
    ));

    let with_prompt = Context {
        system_prompt: Some("p".repeat(60_000)),
        messages,
        tools: None,
    };
    let request_tokens = compaction::estimate_request_context_tokens(&with_prompt).tokens;
    assert!(
        request_tokens >= 16_000,
        "system prompt must be part of the estimate: {request_tokens}"
    );
    assert!(
        compaction::should_compact(&with_prompt, &model, &config),
        "a large system_prompt with no usage record must trigger compaction"
    );
}

#[test]
fn compaction_trigger_counts_large_tools_when_no_usage() {
    // Same shape, but the prompt weight comes entirely from the serialized
    // tool declarations.
    let messages = vec![user_message(&"a".repeat(4_000))];
    let model = fake_model(16_384);
    let config = threshold_config();
    let without_tools = Context {
        system_prompt: None,
        messages: messages.clone(),
        tools: None,
    };
    assert!(!compaction::should_compact(&without_tools, &model, &config));

    let tools = vec![Tool {
        name: "huge".into(),
        description: "t".repeat(60_000),
        parameters: serde_json::json!({"type": "object"}),
        constrained_sampling: None,
    }];
    let prompt_tokens = compaction::estimate_prompt_tokens(None, Some(&tools));
    assert!(
        prompt_tokens >= 14_000,
        "tool declarations must be estimated in tokens: {prompt_tokens}"
    );
    let with_tools = Context {
        system_prompt: None,
        messages,
        tools: Some(tools),
    };
    assert!(
        compaction::should_compact(&with_tools, &model, &config),
        "large tools with no usage record must trigger compaction"
    );
}

#[test]
fn projected_request_threshold_counts_prompt_and_tools_without_usage() {
    // The print host and agent_session thresholds use the projected
    // estimator; it must also pay for the out-of-band prompt when the
    // transcript carries no usage record.
    let messages = vec![user_message(&"a".repeat(4_000))];
    let projected: Vec<compaction::CutEntry> = messages
        .iter()
        .cloned()
        .map(compaction::CutEntry::from_message)
        .collect();
    let branch: Vec<compaction::PathEntryMeta> = Vec::new();
    let messages_only = compaction::estimate_projected_context_tokens(&projected, &branch).tokens;
    assert_eq!(messages_only, 1_000);

    let prompt = "p".repeat(60_000);
    let tools = vec![Tool {
        name: "huge".into(),
        description: "t".repeat(60_000),
        parameters: serde_json::json!({"type": "object"}),
        constrained_sampling: None,
    }];
    let request_tokens = compaction::estimate_request_projected_context_tokens(
        &projected,
        &branch,
        Some(&prompt),
        Some(&tools),
    )
    .tokens;
    assert!(
        request_tokens >= 30_000,
        "projected request estimate must include system_prompt and tools: {request_tokens}"
    );
    assert!(
        compaction::should_compact_tokens(request_tokens, 16_384, &threshold_config()),
        "the projected full-request estimate must clear the threshold"
    );
}

#[test]
fn chunking_budget_is_enforced_in_tokens_for_cjk_spans() {
    let mut span = Vec::new();
    for index in 0..6 {
        span.push(user_message(&format!(
            "第{index}轮：{}",
            "汉字内容".repeat(200)
        )));
        span.push(assistant_message(&"回复".repeat(200)));
    }
    // 4,000 tokens -> 16,000 quarter-token units.
    let chunks = compaction::split_summarization_chunks(span.as_slice(), 16_000);
    assert!(
        chunks.len() > 1,
        "the CJK span must split under the token budget: {chunks:?}"
    );
    let budget = compaction::summarization_chunk_budget(&fake_model(16_384), 4_096);
    for chunk in &chunks {
        assert!(
            compaction::chunk_estimated_tokens(chunk) <= budget.tokens + 1,
            "chunk over token budget: {chunk:?} vs {}",
            budget.tokens
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn context_usage_reports_cjk_tokens_not_quarter_chars() {
    let dir = std::env::temp_dir().join(format!(
        "pir-p0-token-context-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let cwd = dir.to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, &dir).expect("session");
    manager
        .append_message(user_message(&"漢字".repeat(2_000)))
        .expect("append");

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(128_000),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(manager),
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    let usage = agent.context_usage().await.expect("context usage");
    let tokens = usage.tokens.expect("estimated tokens");
    // 4,000 Han chars -> at least 4,000 tokens (old chars/4 gave 1,000).
    assert!(
        tokens >= 4_000,
        "context_usage must count CJK at ~1 token/char: got {tokens}"
    );
    assert_eq!(usage.context_window, 128_000);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// F3b: full-request thresholds (paged + prompt-start gate) and usage priority
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-f3b-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tool with a deliberately huge declaration: the request this session sends
/// carries ~15k tokens of tool JSON even though the transcript is tiny.
struct HugeTool {
    description: &'static str,
}

impl AgentTool for HugeTool {
    fn name(&self) -> &str {
        "huge"
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

fn huge_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(HugeTool {
        description: Box::leak("t".repeat(60_000).into_boxed_str()),
    }));
    registry
}

fn huge_tool_declarations() -> Vec<Tool> {
    vec![Tool {
        name: "huge".into(),
        description: "t".repeat(60_000),
        parameters: serde_json::json!({"type": "object"}),
        constrained_sampling: None,
    }]
}

/// A provider that records every request context. The compaction summarizer is
/// recognizable by its system prompt; every other request is a normal turn.
struct RecordingProvider {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Context>>>,
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![fake_model(16_384)]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let is_summarizer = context.system_prompt.as_deref()
            == Some(compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT);
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = if is_summarizer {
            "## Goal\nsummary"
        } else {
            "ok"
        };
        let message = AssistantMessage {
            api: "fake".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            stop_reason: StopReason::Stop,
            timestamp: 3,
            ..Default::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// Run one prompt against a fresh session with `tools`, returning the provider
/// call count and every request context the provider saw.
async fn run_gate(label: &str, tools: ToolRegistry) -> (usize, Vec<Context>) {
    let dir = TempDir::new(label);
    let cwd = dir.0.to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, &dir.0).expect("session");
    for turn in 0..6 {
        manager
            .append_message(user_message(&format!(
                "question {turn}: {}",
                "q".repeat(80)
            )))
            .expect("user");
        manager
            .append_message(assistant_message(&format!("answer {turn}")))
            .expect("assistant");
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(RecordingProvider {
        calls: Arc::clone(&calls),
        seen: Arc::clone(&seen),
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(16_384),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: Some(manager),
        compaction: Some(compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 4_096,
            keep_recent_tokens: 1,
        }),
        max_turns: 2,
        system_prompt: None,
        cwd: Some(dir.0.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    agent
        .prompt("next question", Arc::new(|_| {}))
        .await
        .expect("prompt");
    let calls = calls.load(Ordering::SeqCst);
    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (calls, seen)
}

fn paged_session(label: &str, turns: usize) -> (TempDir, SessionManager) {
    let dir = TempDir::new(label);
    let cwd = dir.0.to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, &dir.0).expect("session");
    for turn in 0..turns {
        manager
            .append_message(user_message(&format!("turn {turn}: {}", "a".repeat(600))))
            .expect("user");
        // `assistant_message` carries `Usage::default()` (all zero), so no
        // usage record masks the prompt/tools under test.
        manager
            .append_message(assistant_message(&format!("answer {turn}")))
            .expect("assistant");
    }
    let path = manager.path().to_path_buf();
    drop(manager);
    let paged = SessionManager::open_paged(&path, 2 * 1024, 4).expect("paged open");
    (dir, paged)
}

#[tokio::test(flavor = "current_thread")]
async fn paged_threshold_counts_large_prompt_and_tools_when_no_usage() {
    let (_dir, paged) = paged_session("paged-full-request", 6);
    assert!(paged.is_paged(), "the fixture must leave a tail window");
    let model = fake_model(16_384);
    let config = compaction::CompactionConfig {
        enabled: true,
        reserve_tokens: 4_096,
        keep_recent_tokens: 100,
    };
    let prompt = "p".repeat(60_000);
    let tools = huge_tool_declarations();

    let messages_only =
        PagedCompactionInputs::capture(&paged, PagedRequestPrompt::default()).expect("capture");
    assert!(
        !messages_only.threshold_met(&model, &config),
        "the resident messages alone are below the threshold"
    );

    // Each part of the out-of-band prompt must be able to cross the gate on
    // its own, and the full request must also.
    let prompt_only = PagedCompactionInputs::capture(
        &paged,
        PagedRequestPrompt {
            system_prompt: Some(&prompt),
            tools: None,
        },
    )
    .expect("capture");
    assert!(
        prompt_only.threshold_met(&model, &config),
        "a large system_prompt must trigger the paged threshold"
    );
    let tools_only = PagedCompactionInputs::capture(
        &paged,
        PagedRequestPrompt {
            system_prompt: None,
            tools: Some(&tools),
        },
    )
    .expect("capture");
    assert!(
        tools_only.threshold_met(&model, &config),
        "large tool declarations must trigger the paged threshold"
    );
    let full = PagedCompactionInputs::capture(
        &paged,
        PagedRequestPrompt {
            system_prompt: Some(&prompt),
            tools: Some(&tools),
        },
    )
    .expect("capture");
    assert!(full.threshold_met(&model, &config));

    // The free entry point (used by hosts that did not pre-capture inputs)
    // applies exactly the same full-request rule.
    assert!(
        !pir::compaction::paged::threshold_met(
            &paged,
            &model,
            &config,
            PagedRequestPrompt::default()
        )
        .expect("projection"),
        "the free threshold_met must not fall back to messages-only"
    );
    assert!(
        pir::compaction::paged::threshold_met(
            &paged,
            &model,
            &config,
            PagedRequestPrompt {
                system_prompt: Some(&prompt),
                tools: Some(&tools),
            }
        )
        .expect("projection"),
        "the free threshold_met must count system_prompt + tools"
    );

    // The widening pre-check applies the SAME rule: below the threshold the
    // window is not self-contained and must widen; a met threshold keeps the
    // bounded paged path.
    assert!(
        pir::compaction::paged_requires_widening(
            &paged,
            &model,
            &config,
            PagedRequestPrompt::default()
        ),
        "a sub-threshold paged window must widen"
    );
    assert!(
        !pir::compaction::paged_requires_widening(
            &paged,
            &model,
            &config,
            PagedRequestPrompt {
                system_prompt: Some(&prompt),
                tools: Some(&tools),
            }
        ),
        "a threshold-met full request must not widen"
    );

    // `compact_paged_inputs` (the actual run, not just the helper) must accept
    // the full-request decision and report the prompt-inclusive token count.
    let complete: Arc<CompleteFn> = Arc::new(Box::new(|context: Context| {
        Box::pin(async move {
            let text = context
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User(user) => Some(
                        user.content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("SUMMARY\n{text}"),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 4,
                ..Default::default()
            })
        })
    }));
    let compacted =
        pir::compaction::paged::compact_paged_inputs(full, &model, &config, complete, None)
            .await
            .expect("a met full-request threshold must run the paged compaction");
    assert!(
        compacted.result.tokens_before >= 30_000,
        "tokens_before must include system_prompt + tools: {}",
        compacted.result.tokens_before
    );
    assert!(compacted.result.summary.starts_with("SUMMARY"));
    assert!(
        compacted.stats.prefix_bytes > 0,
        "the skipped prefix must be streamed: {:?}",
        compacted.stats
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_start_gate_counts_large_tools_when_no_usage() {
    // The transcript is tiny (~200 tokens); only the 60k-char tool
    // declaration can cross the gate. Before F3b the prompt-start Context
    // carried `tools: None`, so this run never compacted at prompt start.
    let (calls, seen) = run_gate("gate-large-tools", huge_tool_registry()).await;
    assert!(
        calls >= 2,
        "the prompt-start gate must compact before the first model request; calls={calls}"
    );
    assert_eq!(
        seen[0].system_prompt.as_deref(),
        Some(compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT),
        "the first provider request must be the compaction summarizer"
    );
    assert!(
        seen.last()
            .and_then(|context| context.tools.as_ref())
            .is_some_and(|tools| tools.iter().any(|tool| tool.name == "huge")),
        "the actual model request still declares the huge tool"
    );

    // Control: the same transcript without any tool declarations stays below
    // the threshold, proving the tools are what the gate counted.
    let (control_calls, control_seen) = run_gate("gate-no-tools", ToolRegistry::new()).await;
    assert_eq!(
        control_calls, 1,
        "without tool declarations the small transcript must not compact"
    );
    assert_ne!(
        control_seen[0].system_prompt.as_deref(),
        Some(compaction::prompts::SUMMARIZATION_SYSTEM_PROMPT),
        "the control run must reach the model request directly"
    );
}

#[test]
fn request_estimator_prefers_usage_and_does_not_double_count_prompt() {
    let response = AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: "done".into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: 1_000,
            output: 100,
            total_tokens: 1_100,
            ..Usage::default()
        },
        timestamp: 2,
        ..Default::default()
    };
    let messages = vec![
        user_message("question"),
        Message::Assistant(response),
        user_message(&"a".repeat(4_000)),
    ];
    let plain = compaction::estimate_context_tokens(&messages);
    assert_eq!(plain.tokens, 2_100, "usage 1,100 + trailing 1,000");

    let prompt = "p".repeat(60_000);
    let tools = huge_tool_declarations();
    let context = Context {
        system_prompt: Some(prompt.clone()),
        messages: messages.clone(),
        tools: Some(tools.clone()),
    };
    let request = compaction::estimate_request_context_tokens(&context);
    assert_eq!(
        request.tokens, plain.tokens,
        "a usage record already covers the leading prompt: adding it again would double count"
    );
    assert_eq!(request.usage_tokens, 1_100);
    assert_eq!(request.last_usage_index, plain.last_usage_index);
    assert!(
        !compaction::should_compact(&context, &fake_model(16_384), &threshold_config()),
        "usage (2,100) is far below the 12,288-token usable window; the ~30k prompt/tools must not be added"
    );

    // The projected estimator shares the same rule. Give every entry a branch
    // id so the usage record maps to a live path entry (a synthetic entry
    // without an id is treated as unpersisted tail and invalidates usage).
    let projected: Vec<compaction::CutEntry> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let mut entry = compaction::CutEntry::from_message(message.clone());
            entry.id = Some(format!("e{index}"));
            entry
        })
        .collect();
    let branch: Vec<compaction::PathEntryMeta> = messages
        .iter()
        .enumerate()
        .map(|(index, _)| compaction::PathEntryMeta {
            id: Some(format!("e{index}")),
            is_compaction: false,
            is_context_edit: false,
        })
        .collect();
    let plain_projected = compaction::estimate_projected_context_tokens(&projected, &branch);
    let request_projected = compaction::estimate_request_projected_context_tokens(
        &projected,
        &branch,
        Some(&prompt),
        Some(&tools),
    );
    assert_eq!(request_projected.tokens, plain_projected.tokens);
    assert_eq!(
        request_projected.last_usage_index,
        plain_projected.last_usage_index
    );
}
