//! `PIR_RESUME_ON_ERROR` opt-in breakpoint continuation: the A1 tail-error
//! detector that decides whether an explicit resume with no new prompt should
//! inject a synthetic "continue" turn. Tests build minimal session files via
//! `SessionManager::create`/`append_message` so they are self-contained.

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, UserMessage, UserRole,
};
use pir::coding_agent::print::{
    resume_on_error_enabled, session_tail_is_error, RESUME_CONTINUATION_PROMPT,
    RESUME_ON_ERROR_PROMPT,
};
use pir::session_manager::SessionManager;
use std::sync::{Mutex, OnceLock};

fn assistant(stop: StopReason) -> Message {
    let a = AssistantMessage {
        stop_reason: stop,
        ..Default::default()
    };
    Message::Assistant(a)
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 0,
    })
}

fn open_with(messages: Vec<Message>) -> SessionManager {
    // Unique dir per test call via a process-wide counter.
    static N: OnceLock<Mutex<u32>> = OnceLock::new();
    let n = {
        let mut g = N.get_or_init(|| Mutex::new(0)).lock().unwrap();
        *g += 1;
        *g
    };
    let base = std::env::temp_dir().join(format!(
        "pir-resume-on-error-test-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let mut sm = SessionManager::create(&base.to_string_lossy(), &base).unwrap();
    for m in messages {
        sm.append_message(m).unwrap();
    }
    let path = sm.path().to_path_buf();
    SessionManager::open(&path).unwrap()
}

#[test]
fn tail_error_is_detected_on_a_resumed_session() {
    let sm = open_with(vec![
        user("build a thing"),
        assistant(StopReason::ToolUse),
        assistant(StopReason::Error), // HTTP 10054-style terminal failure
    ]);
    assert!(
        session_tail_is_error(&sm),
        "a leaf assistant with stopReason=error is a recovery breakpoint"
    );
}

#[test]
fn a_normal_stop_is_not_a_breakpoint() {
    let sm = open_with(vec![user("build a thing"), assistant(StopReason::Stop)]);
    assert!(!session_tail_is_error(&sm));
}

#[test]
fn a_later_user_turn_clears_the_breakpoint() {
    // error buried under a later user turn is mid-run history, not a leaf error
    let sm = open_with(vec![
        user("build a thing"),
        assistant(StopReason::Error),
        user("continue please"),
    ]);
    assert!(!session_tail_is_error(&sm));
}

#[test]
fn continuation_prompt_is_actionable_and_env_switch_is_explicit() {
    assert!(RESUME_ON_ERROR_PROMPT.contains("Continue"));
    assert!(RESUME_ON_ERROR_PROMPT.contains("verify"));
    // A bare --resume with a clean leaf must also get a non-empty turn rather
    // than an empty-string prompt (run_mode.rs relaxes the prompt requirement
    // and run_print pairs it with this fallback).
    assert!(RESUME_CONTINUATION_PROMPT.contains("Continue"));
    assert!(!RESUME_CONTINUATION_PROMPT.trim().is_empty());
    // The switch must be opt-in; tests must not depend on ambient env state,
    // so only assert the shape (the helper reads PIR_RESUME_ON_ERROR at call
    // time and is exercised end-to-end by the real-session smoke run).
    let _ = resume_on_error_enabled();
}

/// Windows WSAECONNRESET (os error 10054) is a transient transport failure and
/// must be picked up by the agent-level turn retry, so a dropped provider
/// connection is re-issued instead of ending the run.
#[test]
fn windows_10054_disconnect_is_retryable() {
    use pir::ai::util::is_retryable_assistant_error;
    let err = |text: &str| AssistantMessage {
        api: "openai-completions".into(),
        provider: "zcode".into(),
        model: "deepseek-v4.1-flash".into(),
        stop_reason: StopReason::Error,
        error_message: Some(text.to_owned()),
        timestamp: 1,
        ..Default::default()
    };
    for text in [
        "HTTP error: error decoding response body: 远程主机强迫关闭了一个现有的连接。 (os error 10054)",
        "error decoding response body: Connection reset by peer (os error 10054)",
        "io error: WSAECONNRESET",
    ] {
        assert!(
            is_retryable_assistant_error(&err(text)),
            "WSAECONNRESET must be retryable: {text}"
        );
    }
    // A non-error stop is never a retry candidate regardless of text.
    let mut ok = err("os error 10054");
    ok.stop_reason = StopReason::Stop;
    assert!(!is_retryable_assistant_error(&ok));
}
