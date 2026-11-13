//! Unit tests for `pir::agent_session` helpers that the public prompt path
//! cannot reach directly (ephemeral history trimming).

use pir::agent_session::trim_ephemeral_history;
use pir::ai::types::{AssistantMessage, ContentBlock, Message, TextContent, UserMessage, UserRole};

/// Local copy of the crate-private `user_message` fixture in
/// `src/agent_session.rs`; integration tests cannot see it.
fn user_message(text: &str, images: Vec<ContentBlock>) -> Message {
    let mut content = vec![ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })];
    content.extend(images);
    Message::User(UserMessage {
        role: UserRole::User,
        content,
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: pir::ai::util::now_ms(),
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    })
}

fn message_text(message: &Message) -> &str {
    match message {
        Message::User(user) | Message::System(user) | Message::Developer(user) => {
            match user.content.first() {
                Some(ContentBlock::Text(text)) => &text.text,
                _ => "",
            }
        }
        Message::Assistant(assistant) => match assistant.content.first() {
            Some(ContentBlock::Text(text)) => &text.text,
            _ => "",
        },
        Message::ToolResult(_) => "",
    }
}

#[test]
fn keeps_newest_messages_within_count_cap() {
    let mut history: Vec<Message> = (0..8)
        .map(|index| user_message(&format!("m{index}"), Vec::new()))
        .collect();

    trim_ephemeral_history(&mut history, 3, u64::MAX);

    let texts: Vec<&str> = history.iter().map(message_text).collect();
    assert_eq!(texts, ["m5", "m6", "m7"]);
}

#[test]
fn keeps_newest_messages_within_token_cap() {
    // 1,000 chars ≈ 250 estimated tokens each (chars/4).
    let mut history: Vec<Message> = (0..4)
        .map(|_| user_message(&"x".repeat(1_000), Vec::new()))
        .collect();

    trim_ephemeral_history(&mut history, usize::MAX, 500);

    assert_eq!(history.len(), 2);
    assert!(history
        .iter()
        .all(|message| matches!(message, Message::User(_))));
}

#[test]
fn retained_window_starts_at_a_user_turn_boundary() {
    let mut history = vec![
        user_message("u1", Vec::new()),
        assistant_message("a1"),
        user_message("u2", Vec::new()),
        assistant_message("a2"),
    ];

    // The count cap would retain `a1, u2, a2`; the boundary rule drops the
    // partial turn so the window starts at `u2`.
    trim_ephemeral_history(&mut history, 3, u64::MAX);

    let texts: Vec<&str> = history.iter().map(message_text).collect();
    assert_eq!(texts, ["u2", "a2"]);
}

#[test]
fn empty_history_is_unchanged() {
    let mut history = Vec::new();
    trim_ephemeral_history(&mut history, 1, 1);
    assert!(history.is_empty());
}

// ---------------------------------------------------------------------------
// PIR-BIN-075: the ephemeral-history cap must announce what it drops
// ---------------------------------------------------------------------------

/// The message cap is the one that binds here: 1 100 user messages against the
/// production 1 024 cap drops the 76 oldest, and the report says so.
#[test]
fn trim_reports_the_dropped_count_and_the_message_cap() {
    let mut history: Vec<Message> = (0..1_100)
        .map(|index| user_message(&format!("m{index}"), Vec::new()))
        .collect();

    let trim = trim_ephemeral_history(
        &mut history,
        pir::agent_session::EPHEMERAL_HISTORY_MAX_MESSAGES,
        u64::MAX,
    );

    assert!(
        trim.trimmed(),
        "crossing the message cap must report a non-empty trim"
    );
    assert_eq!(trim.dropped_messages, 76, "the 76 OLDEST messages go");
    assert_eq!(trim.retained_messages, 1_024);
    assert_eq!(trim.reason, Some("messages"));
    assert_eq!(history.len(), 1_024);
    // The newest survive, the oldest are gone.
    assert_eq!(message_text(&history[0]), "m76");
    assert_eq!(message_text(&history[1_023]), "m1099");

    // The user-facing line names the count AND the cap, so the user knows what
    // they lost rather than only that something is missing.
    let notice = pir::agent_session::ephemeral_trim_notice(trim, trim.dropped_messages);
    assert_eq!(
        notice,
        "pir: warning: --no-session history trimmed: dropped the 76 oldest message(s), kept 1024 \
         (ephemeral cap: 1024 messages); 76 message(s) dropped in total since this session \
         started. Persisted sessions are unaffected."
    );
}

/// The token cap reports itself as the reason (the 1 500-char shape the
/// endurance lane measured), so a client can tell which bound applied.
#[test]
fn trim_reports_the_token_cap() {
    // 400 chars ≈ 100 estimated tokens each; a 500-token cap keeps 5.
    let mut history: Vec<Message> = (0..8)
        .map(|_| user_message(&"x".repeat(400), Vec::new()))
        .collect();

    let trim = trim_ephemeral_history(&mut history, usize::MAX, 500);

    assert_eq!(trim.dropped_messages, 3);
    assert_eq!(trim.retained_messages, 5);
    assert_eq!(trim.reason, Some("tokens"));
    let notice = pir::agent_session::ephemeral_trim_notice(trim, 3);
    assert!(
        notice.contains("(ephemeral cap: 100000 estimated tokens)"),
        "the notice must name the token cap: {notice}"
    );
}

/// No false positives: a transcript that fits reports nothing, so the session
/// layer skips the announcement (and its counter stays 0).
#[test]
fn trim_within_the_caps_reports_no_trim() {
    let mut history: Vec<Message> = (0..4)
        .map(|index| user_message(&format!("m{index}"), Vec::new()))
        .collect();

    let trim = trim_ephemeral_history(
        &mut history,
        pir::agent_session::EPHEMERAL_HISTORY_MAX_MESSAGES,
        pir::agent_session::EPHEMERAL_HISTORY_MAX_TOKENS,
    );

    assert!(!trim.trimmed());
    assert_eq!(trim.dropped_messages, 0);
    assert_eq!(trim.reason, None);
    assert_eq!(history.len(), 4);
}

/// The trim advances to a user boundary, and the reported dropped count is the
/// number actually drained (not the pre-boundary candidate).
#[test]
fn trim_reports_the_count_after_the_user_boundary() {
    let mut history = vec![
        user_message("u1", Vec::new()),
        assistant_message("a1"),
        user_message("u2", Vec::new()),
        assistant_message("a2"),
    ];

    let trim = trim_ephemeral_history(&mut history, 3, u64::MAX);

    assert_eq!(trim.dropped_messages, 2, "u1 and its assistant went");
    assert_eq!(trim.retained_messages, 2);
    assert_eq!(message_text(&history[0]), "u2");
}

// ---------------------------------------------------------------------------
// PIR-BIN-057: the session's request options carry `settings.retry.provider.*`
// ---------------------------------------------------------------------------

/// Minimal facade model: no catalog, no credentials, no transport (the request
/// options are inspected, never sent).
fn retry_model() -> pir::ai::types::Model {
    use pir::ai::types::{Model, ModelCost, ModelCostRates};
    Model {
        id: "retry-model".into(),
        name: "Retry model".into(),
        api: "openai-chat".into(),
        provider: "retry-test".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
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

fn retry_session(agent_dir: &std::path::Path) -> pir::agent_session::AgentSession {
    use pir::agent::tool_execution::ToolRegistry;
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::provider::ProviderRegistry;
    use pir::ai::types::ThinkingLevel;
    AgentSession::new_with_agent_dir(
        AgentSessionConfig {
            performance: Default::default(),
            provider_registry: std::sync::Arc::new(ProviderRegistry::default()),
            model: retry_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: Some("retry wiring".to_owned()),
            cwd: Some(agent_dir.to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        agent_dir,
    )
}

/// `settings.retry.provider.{maxRetries,maxRetryDelayMs,timeoutMs}` must reach
/// the `StreamOptions` of every session request (upstream sdk.ts:341-352).
/// The session's settings are read from ITS agent dir, so this needs no
/// process environment.
#[test]
fn session_stream_options_carry_provider_retry_settings() {
    let dir = std::env::temp_dir().join(format!(
        "pir-retry-wiring-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "retry": { "provider": { "maxRetries": 3, "maxRetryDelayMs": 5000, "timeoutMs": 1000 } } }"#,
    )
    .expect("write settings.json");

    let session = retry_session(&dir);
    let options = session.stream_options_for_test();
    assert_eq!(
        options.max_retries,
        Some(3),
        "retry.provider.maxRetries must reach StreamOptions"
    );
    assert_eq!(
        options.max_retry_delay_ms,
        Some(5_000),
        "retry.provider.maxRetryDelayMs must reach StreamOptions"
    );
    assert_eq!(
        options.timeout_ms,
        Some(1_000),
        "retry.provider.timeoutMs must reach StreamOptions"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The default path is unchanged (v8): with no `retry.provider` block the
/// session request carries `None` for all three knobs, so the transport keeps
/// its compiled 2 retries / 60 s cap / no total timeout.
#[test]
fn session_stream_options_keep_transport_defaults_without_settings() {
    let dir = std::env::temp_dir().join(format!(
        "pir-retry-defaults-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(dir.join("settings.json"), "{}").expect("write settings.json");

    let session = retry_session(&dir);
    let options = session.stream_options_for_test();
    assert_eq!(options.max_retries, None);
    assert_eq!(options.max_retry_delay_ms, None);
    assert_eq!(options.timeout_ms, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The explicit embedder override (`set_retry`, used by the SDK builder) still
/// wins over the settings, as upstream's `options.maxRetries ??` layering
/// requires: a settings file must not silently re-enable retries an embedder
/// switched off.
#[test]
fn session_set_retry_overrides_the_settings_file() {
    let dir = std::env::temp_dir().join(format!(
        "pir-retry-override-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "retry": { "provider": { "maxRetries": 3, "maxRetryDelayMs": 5000 } } }"#,
    )
    .expect("write settings.json");

    let session = retry_session(&dir);
    session.set_retry(Some(0), Some(20));
    let options = session.stream_options_for_test();
    assert_eq!(options.max_retries, Some(0));
    assert_eq!(options.max_retry_delay_ms, Some(20));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `set_auto_retry(false)` stays the hard off switch for the transport budget
/// even with a configured `maxRetries`.
#[test]
fn session_auto_retry_off_beats_the_settings_budget() {
    let dir = std::env::temp_dir().join(format!(
        "pir-retry-auto-off-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "retry": { "provider": { "maxRetries": 3 } } }"#,
    )
    .expect("write settings.json");

    let session = retry_session(&dir);
    session.set_auto_retry_enabled(false);
    let options = session.stream_options_for_test();
    assert_eq!(options.max_retries, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}
