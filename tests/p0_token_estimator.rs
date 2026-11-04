//! P0-1: Unicode-aware token estimation (CJK fairness) across the shared
//! compaction trigger, chunking, and `context_usage` paths.

use std::sync::Arc;

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::compaction;
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
