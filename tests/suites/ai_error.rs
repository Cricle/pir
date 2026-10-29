//! Tests for AI error types.

use pir::ai::error::{AiError, AiResult, TurnRetryClass};

#[test]
fn stream_not_terminated_display() {
    let e = AiError::StreamNotTerminated;
    assert!(e.to_string().contains("stream ended without"));
    assert!(!e.to_string().is_empty());
}

#[test]
fn unknown_provider_display() {
    let e = AiError::UnknownProvider("ollama".into());
    assert!(e.to_string().contains("ollama"));
}

#[test]
fn retry_exhausted_display() {
    let e = AiError::RetryExhausted {
        attempts: 3,
        message: "timeout".into(),
    };
    let s = e.to_string();
    assert!(s.contains("3"));
    assert!(s.contains("timeout"));
}

#[test]
fn aborted_stop_reason() {
    let e = AiError::Aborted;
    assert_eq!(e.stop_reason(), pir::ai::types::StopReason::Aborted);
    assert_eq!(e.user_message(), "Request was aborted");
}

#[test]
fn non_retryable_stop_reason() {
    let e = AiError::NonRetryable("quota".into());
    assert_eq!(e.stop_reason(), pir::ai::types::StopReason::Error);
}

#[test]
fn retryable_provider_frame_keeps_text_and_retries_turn() {
    let e = AiError::Retryable("Rate limit reached".into());
    assert_eq!(e.stop_reason(), pir::ai::types::StopReason::Error);
    assert_eq!(e.user_message(), "Rate limit reached");
    assert_eq!(e.turn_retry_class(), TurnRetryClass::RetryableTurn);
}

#[test]
fn stream_truncated_is_retryable_turn() {
    let e = AiError::StreamTruncated("stream ended early".into());
    assert_eq!(e.turn_retry_class(), TurnRetryClass::RetryableTurn);
}

#[test]
fn json_parse_error_stays_non_retryable() {
    let json_err = serde_json::from_str::<()>("not json").unwrap_err();
    let e = AiError::Json(json_err);
    assert_eq!(e.turn_retry_class(), TurnRetryClass::NonRetryable);
}

#[test]
fn retry_exhausted_truncation_never_splits_a_codepoint() {
    // 360 bytes of 3-byte characters: a byte slice at index 200 lands mid
    // codepoint and used to panic.
    let e = AiError::RetryExhausted {
        attempts: 2,
        message: "错误".repeat(60),
    };
    let rendered = e.user_message();
    assert!(rendered.ends_with("..."), "{rendered}");
    assert!(rendered.is_char_boundary(rendered.len()));
}

#[test]
fn retry_exhausted_user_message_truncated() {
    let long = "x".repeat(300);
    let e = AiError::RetryExhausted {
        attempts: 5,
        message: long.clone(),
    };
    let msg = e.user_message();
    assert!(msg.len() <= 203, "msg len: {}", msg.len());
    assert!(msg.ends_with("..."));
}

#[test]
fn retry_exhausted_user_message_short() {
    let e = AiError::RetryExhausted {
        attempts: 2,
        message: "brief".into(),
    };
    assert_eq!(e.user_message(), "brief");
}

#[test]
fn io_error_from_source() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let e = AiError::Io(io_err);
    assert!(e.to_string().contains("file not found"));
}

#[test]
fn json_error_from_source() {
    let json_err = serde_json::from_str::<()>("not json").unwrap_err();
    let e = AiError::Json(json_err);
    assert!(e.to_string().contains("not json") || e.to_string().contains("expected"));
}

#[test]
fn ai_result_type_alias() {
    fn ok() -> AiResult<i32> {
        Ok(42)
    }
    assert_eq!(ok().unwrap(), 42);
}

// ---------------------------------------------------------------------------
// Retry-class carrier removal (CL38 R3: foreign/duplicated markers)
// ---------------------------------------------------------------------------

fn carrier_message() -> pir::ai::types::AssistantMessage {
    pir::ai::types::AssistantMessage {
        api: "test-api".into(),
        provider: "test".into(),
        model: "test-model".into(),
        stop_reason: pir::ai::types::StopReason::Error,
        error_message: Some("stream ended without a terminal event".into()),
        timestamp: 1,
        ..Default::default()
    }
}

fn push_carrier(
    message: &mut pir::ai::types::AssistantMessage,
    code: Option<&str>,
    diag_type: &str,
) {
    message.diagnostics.push(pir::ai::types::DiagnosticInfo {
        diag_type: diag_type.to_string(),
        timestamp: 1,
        error_message: None,
        error_name: None,
        error_stack: None,
        error_code: code.map(|code| serde_json::Value::String(code.to_string())),
        details: None,
    });
}

/// A duplicated carrier must not survive the take: a JS `streamSimple`
/// provider hands its events over whole, so it can plant or duplicate the
/// marker. Only the FIRST carrier decides the verdict; every carrier is
/// removed.
#[test]
fn take_turn_retry_class_removes_every_carrier_and_first_wins() {
    let mut message = carrier_message();
    push_carrier(
        &mut message,
        Some("non_retryable"),
        pir::ai::error::RETRY_CLASS_DIAGNOSTIC,
    );
    // A foreign duplicate AFTER the genuine marker: it must neither survive
    // the take nor override the verdict.
    push_carrier(
        &mut message,
        Some("retryable_turn"),
        pir::ai::error::RETRY_CLASS_DIAGNOSTIC,
    );
    // An unrelated diagnostic is not a carrier and stays.
    push_carrier(&mut message, None, "provider_warning");
    // A duplicated carrier BEFORE the genuine marker decides (first wins).
    message.diagnostics.insert(
        0,
        pir::ai::types::DiagnosticInfo {
            diag_type: pir::ai::error::RETRY_CLASS_DIAGNOSTIC.to_string(),
            timestamp: 0,
            error_message: None,
            error_name: None,
            error_stack: None,
            error_code: Some(serde_json::Value::String("transport_exhausted".into())),
            details: None,
        },
    );

    assert_eq!(
        pir::ai::error::take_turn_retry_class(&mut message),
        Some(pir::ai::error::TurnRetryClass::TransportExhausted),
        "the FIRST carrier decides, deterministically"
    );
    assert!(
        !message
            .diagnostics
            .iter()
            .any(|d| d.diag_type == pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
        "no carrier may survive the take: {:?}",
        message.diagnostics
    );
    assert_eq!(
        message.diagnostics.len(),
        1,
        "the unrelated diagnostic survives: {:?}",
        message.diagnostics
    );
    assert_eq!(message.diagnostics[0].diag_type, "provider_warning");
    assert_eq!(
        message.error_message.as_deref(),
        Some("stream ended without a terminal event"),
        "the real error text is untouched"
    );
}

/// A malformed FIRST carrier (no/unrecognized `error.code`) fails closed: the
/// take can never be "rescued" by a well-formed duplicate, and every carrier
/// is still removed.
#[test]
fn take_turn_retry_class_fails_closed_on_a_malformed_first_carrier() {
    let mut message = carrier_message();
    push_carrier(&mut message, None, pir::ai::error::RETRY_CLASS_DIAGNOSTIC);
    push_carrier(
        &mut message,
        Some("retryable_turn"),
        pir::ai::error::RETRY_CLASS_DIAGNOSTIC,
    );

    assert_eq!(
        pir::ai::error::take_turn_retry_class(&mut message),
        None,
        "an unparseable leading carrier is not retryable"
    );
    assert!(
        message.diagnostics.is_empty(),
        "both carriers are removed: {:?}",
        message.diagnostics
    );
}

/// `strip_turn_retry_class` removes every carrier without interpreting any of
/// them and reports the count, leaving unrelated diagnostics alone.
#[test]
fn strip_turn_retry_class_removes_all_carriers_and_counts_them() {
    let mut message = carrier_message();
    push_carrier(&mut message, None, "provider_warning");
    push_carrier(
        &mut message,
        Some("retryable_turn"),
        pir::ai::error::RETRY_CLASS_DIAGNOSTIC,
    );
    push_carrier(
        &mut message,
        Some("garbage"),
        pir::ai::error::RETRY_CLASS_DIAGNOSTIC,
    );

    assert_eq!(pir::ai::error::strip_turn_retry_class(&mut message), 2);
    assert_eq!(message.diagnostics.len(), 1);
    assert_eq!(message.diagnostics[0].diag_type, "provider_warning");
    assert_eq!(
        pir::ai::error::strip_turn_retry_class(&mut message),
        0,
        "idempotent"
    );
}
