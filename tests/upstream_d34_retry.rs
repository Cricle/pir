//! D34 — gateway upstream-failure wording is retryable (upstream #10920).
//!
//! The gateway relays some upstream failures as OpenAI Responses
//! `response.failed` codes with no HTTP status; the message then carries the
//! code (or prose) rather than a 5xx. Before this drift item the classifier
//! failed them fast, so a transient gateway hiccup ended the session.
//!
//! Source commits: `7efebff18` ("Unable to forward upstream stream") and
//! `698a2d23b` (`upstream_error`, `upstream_stream_read_error`,
//! `stream was interrupted`, HTTP/2 stream failure).

use pir::ai::types::{AssistantMessage, StopReason};
use pir::ai::util::is_retryable_assistant_error;

fn error_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "openai-responses".into(),
        provider: "gateway".into(),
        model: "test-model".into(),
        stop_reason: StopReason::Error,
        error_message: Some(text.to_owned()),
        timestamp: 1,
        ..Default::default()
    }
}

#[test]
fn d34_gateway_upstream_failures_are_retryable() {
    for text in [
        // #10920: relayed as a `response.failed` event without an error code.
        "unknown: Unable to forward upstream stream",
        // The error-code shapes the gateway emits for the same condition.
        "upstream_error: Upstream request failed",
        "upstream_stream_read_error: Upstream response stream was interrupted",
        // Prose forms (capitalization varies with the upstream SDK).
        "Upstream HTTP/2 stream failed",
        "Stream was interrupted",
        // D37 (#10996): node-fetch system errors, e.g. the Google ADC OAuth
        // token refresh used by google-vertex. The reason is often empty.
        "request to https://oauth2.googleapis.com/token failed, reason:",
        "request to https://oauth2.googleapis.com/token failed, reason: getaddrinfo ENOTFOUND",
    ] {
        assert!(
            is_retryable_assistant_error(&error_message(text)),
            "expected retryable: {text}"
        );
    }
}

#[test]
fn d34_billing_limits_still_win_over_the_new_wording() {
    // The non-retryable limit pattern is consulted first: a gateway wording
    // next to an exhaustion code must stay fail-fast.
    for text in [
        "upstream_stream_read_error: insufficient_quota",
        "Unable to forward upstream stream: quota exceeded",
    ] {
        assert!(
            !is_retryable_assistant_error(&error_message(text)),
            "expected fail-fast: {text}"
        );
    }
}

#[test]
fn d34_stop_reasons_are_unaffected() {
    // The classifier only fires for `stopReason: "error"`; a completed turn
    // whose text happens to mention the wording is never retried.
    let mut message = error_message("Unable to forward upstream stream");
    message.stop_reason = StopReason::Stop;
    assert!(!is_retryable_assistant_error(&message));
}
