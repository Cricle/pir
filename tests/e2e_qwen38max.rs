//! Real-world E2E test using the pi configuration's qwen3.8-max model.
//! Requires: ~/.pi/agent/models.json with qwen3.8-max configured.
//! Tests: real HTTP request, response parsing, usage, stop reason.

use pir::ai::providers::load_pi_config;
use pir::ai::types::*;

// Real-network integration tests are opt-in: they are `#[ignore]`d, and run
// only with PIR_RUN_LIVE_E2E=1
// (`cargo test --test e2e_qwen38max -- --ignored`).

#[tokio::test]
#[ignore = "requires PIR_RUN_LIVE_E2E=1 and a live qwen3.8-max endpoint"]
async fn qwen38max_completes_python_task() {
    if std::env::var("PIR_RUN_LIVE_E2E").ok().as_deref() != Some("1") {
        return;
    }
    let configured = load_pi_config().expect("pi config should load");
    let Some(configured) = configured else {
        eprintln!("SKIP: no pi config found at ~/.pi/agent/models.json");
        return;
    };

    let task = "\nWrite a Python function that reads a file and counts word frequency. Return only the code.";

    let ctx = Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: task.into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: pir::ai::util::now_ms(),
            sections: None,
        })],
        ..Context::default()
    };

    let opts = StreamOptions {
        api_key: configured.api_key.clone(),
        ..StreamOptions::default()
    };

    let stream = configured
        .registry
        .stream_unchecked(configured.model.clone(), ctx, opts);
    let result = stream.result().await;

    match result {
        Some(msg) => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::Text(t) = block {
                        Some(t.text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            assert!(!text.is_empty(), "response should contain code");
            assert!(
                text.contains("def") || text.contains("import"),
                "response should contain Python code, got: {text:.200}"
            );
            assert!(
                msg.usage.total_tokens > 0,
                "usage total_tokens should be > 0"
            );
            assert!(
                matches!(msg.stop_reason, StopReason::Stop | StopReason::Length),
                "stop reason should be stop or length, got: {:?}",
                msg.stop_reason
            );

            println!("=== qwen3.8-max real task result ===");
            println!("Input tokens: {}", msg.usage.input);
            println!("Output tokens: {}", msg.usage.output);
            println!("Total tokens: {}", msg.usage.total_tokens);
            println!("Stop reason: {:?}", msg.stop_reason);
            println!("Response:\n{text}");
        }
        None => {
            eprintln!("qwen3.8-max returned no final message");
            panic!("Real API call should succeed with valid pi config");
        }
    }
}

#[tokio::test]
#[ignore = "requires PIR_RUN_LIVE_E2E=1 and a live qwen3.8-max endpoint"]
async fn qwen38max_completes_analysis_task() {
    if std::env::var("PIR_RUN_LIVE_E2E").ok().as_deref() != Some("1") {
        return;
    }
    let configured = load_pi_config().expect("pi config should load");
    let Some(configured) = configured else {
        eprintln!("SKIP: no pi config found");
        return;
    };

    let task =
        "\nWhat are the three most important concepts in Rust's ownership system? Explain briefly.";

    let ctx = Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: task.into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: pir::ai::util::now_ms(),
            sections: None,
        })],
        ..Context::default()
    };

    let opts = StreamOptions {
        api_key: configured.api_key.clone(),
        ..StreamOptions::default()
    };

    let stream = configured
        .registry
        .stream_unchecked(configured.model.clone(), ctx, opts);
    let result = stream.result().await;

    match result {
        Some(msg) => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::Text(t) = block {
                        Some(t.text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            assert!(!text.is_empty(), "response should not be empty");
            assert!(
                text.to_lowercase().contains("ownership")
                    || text.to_lowercase().contains("borrow")
                    || text.to_lowercase().contains("lifetime"),
                "response should discuss Rust ownership, got: {text:.200}"
            );

            println!("=== qwen3.8-max analysis task ===");
            println!("Input tokens: {}", msg.usage.input);
            println!("Output tokens: {}", msg.usage.output);
            println!("Total tokens: {}", msg.usage.total_tokens);
            println!("Stop reason: {:?}", msg.stop_reason);
            println!("Response:\n{text}");
        }
        None => {
            eprintln!("qwen3.8-max returned no final message");
            panic!("Real API call should succeed with valid pi config");
        }
    }
}
