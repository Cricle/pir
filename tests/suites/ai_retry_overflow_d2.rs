//! D2 — retry/overflow classification boundaries (audit #3/#4/#5; upstream
//! `packages/ai/src/utils/retry.ts:30,43,99` and
//! `packages/ai/src/utils/overflow.ts:64-66`).
//!
//! - `retry.ts:43` guards transient HTTP status codes with ASCII lookaround
//!   assertions, so ids (`req_011CU503xyz`) and byte counts
//!   (`5242880 bytes`) are not status codes (#11057). pir scans candidates
//!   and checks the adjacent characters instead of using lookaround.
//! - `retry.ts:99` adds the Codex CLI mid-stream disconnect wording;
//!   `retry.ts:30` adds the ChatGPT Business workspace usage limit, which
//!   must fail fast.
//! - `overflow.ts:64-66` treats a bare HTTP 413 (or a
//!   `<provider> API error (413):` prefix) as request-size overflow for every
//!   provider, so Copilot large-image / large-body failures take the
//!   compact-and-retry path (#11016).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::agent::tool_execution::BoxFuture;
use pir::ai::error::{AiError, AiResult};
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    ProviderStreamHooks, StopReason, TextContent, Usage, UserMessage, UserRole,
};
use pir::ai::util::is_context_overflow;
use pir::compaction::prompts::COMPACTION_SUMMARY_PREFIX;
use pir::compaction::{recover_from_overflow, CompactionConfig, CompleteFn};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn error_message(provider: &str, text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "openai-completions".into(),
        provider: provider.into(),
        model: "test-model".into(),
        stop_reason: StopReason::Error,
        error_message: Some(text.to_string()),
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn stop_assistant(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        api: "openai-responses".into(),
        provider: "github-copilot".into(),
        model: "copilot-test-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage::default(),
        timestamp: 2,
        ..AssistantMessage::default()
    })
}

fn copilot_model() -> Model {
    Model {
        id: "copilot-test-model".into(),
        name: "Copilot test model".into(),
        api: "openai-responses".into(),
        provider: "github-copilot".into(),
        base_url: "https://copilot.invalid".into(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".into()],
        input_limits: None,
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 1_000_000,
        max_tokens: 128_000,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        headers: None,
        prompt_cache: None,
        thinking_token_budget_field: None,
        compat: None,
        model_type: None,
    }
}

fn fake_complete(summary: &'static str) -> CompleteFn {
    Box::new(move |_context| {
        Box::pin(async move {
            Ok(AssistantMessage {
                api: "test".into(),
                provider: "test".into(),
                model: "test-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: summary.into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..AssistantMessage::default()
            })
        })
    })
}

// ---------------------------------------------------------------------------
// #3 — HTTP status codes must be standalone tokens
// ---------------------------------------------------------------------------

#[test]
fn d2_status_codes_match_only_as_standalone_tokens() {
    use pir::ai::util::is_retryable_assistant_error;

    // Upstream-visible status mentions stay retryable, including at the
    // string edges and across line breaks.
    for text in [
        "HTTP 503",
        "status: 429",
        "gateway returned 502",
        "server responded 500",
        "error 504 from gateway",
        "Cloudflare 520",
        "gave up after 524",
        "503",
        "retry later (429).",
        "failed\n500\n",
        // Upstream's ASCII-only lookaround treats non-ASCII characters as
        // delimiters, so these still match.
        "错误503错误",
    ] {
        assert!(
            is_retryable_assistant_error(&error_message("openai", text)),
            "expected retryable: {text}"
        );
    }

    // #11057 regressions: status digits inside ids / longer numbers are not
    // status codes.
    for text in [
        // Request id used by upstream's retry test.
        r#"401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011CU503xyz"}"#,
        // Byte count used by upstream's retry test ("5242880 bytes").
        "400 image exceeds 5 MB maximum: 5316784 bytes > 5242880 bytes",
        "job-500abc finished",
        "session 5021 active",
        "gateway 5200",
        "v1.5030",
        "id 4294967295",
        "x524y",
    ] {
        assert!(
            !is_retryable_assistant_error(&error_message("openai", text)),
            "expected NOT retryable: {text}"
        );
    }
}

#[test]
fn d2_status_code_scan_still_defers_to_limit_errors() {
    use pir::ai::util::is_retryable_assistant_error;

    // The non-retryable quota/billing pattern is consulted before the status
    // scan, so a real 429 carrying an exhaustion code still fails fast.
    for text in [
        "429 insufficient_quota",
        "HTTP 503 quota exceeded",
        "upstream 500 billing failure",
        "status 429: GoUsageLimitError",
    ] {
        assert!(
            !is_retryable_assistant_error(&error_message("openai", text)),
            "expected fail-fast: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// #4 — missing retry-table entries
// ---------------------------------------------------------------------------

#[test]
fn d2_stream_disconnect_before_completion_is_retryable() {
    use pir::ai::util::is_retryable_assistant_error;

    // retry.ts:99 (#11057): Codex CLI mid-stream disconnect relayed by
    // proxies.
    for text in [
        "stream disconnected before completion: stream closed before response.completed",
        "Stream disconnected before completion",
    ] {
        assert!(
            is_retryable_assistant_error(&error_message("openai-codex", text)),
            "expected retryable: {text}"
        );
    }
}

#[test]
fn d2_workplace_subscription_limit_fails_fast() {
    use pir::ai::util::is_retryable_assistant_error;

    // retry.ts:30: a ChatGPT Business workspace usage limit can only be
    // raised by the admin, so it must not be retried even when delivered as
    // an HTTP 429.
    for text in [
        "subscription_sharing_workplace_individual_usage_limit_exceeded",
        r#"OpenAI API error (429): {"code":"subscription_sharing_workplace_individual_usage_limit_exceeded","message":"Usage limit reached."}"#,
        "stream disconnected before completion: subscription_sharing_workplace_individual_usage_limit_exceeded",
    ] {
        assert!(
            !is_retryable_assistant_error(&error_message("openai", text)),
            "expected fail-fast: {text}"
        );
    }

    // The pre-existing shared/subscription entries keep their dispositions.
    assert!(!is_retryable_assistant_error(&error_message(
        "openai",
        "subscription_sharing_usage_limit_exceeded"
    )));
    for text in [
        "subscription_sharing_usage_unavailable: Usage cannot be checked.",
        "subscription_sharing_user_unavailable: User cannot be loaded.",
    ] {
        assert!(
            is_retryable_assistant_error(&error_message("openai", text)),
            "expected retryable: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// #5 — provider-independent HTTP 413 overflow
// ---------------------------------------------------------------------------

#[test]
fn d2_generic_413_is_request_size_overflow() {
    // The provider-independent 413 rule from upstream overflow.ts:64-66.
    for (provider, text) in [
        (
            "github-copilot",
            r#"github-copilot API error (413): {"message":"failed to parse request","code":""}"#,
        ),
        // Any provider, not just Copilot.
        (
            "custom-provider",
            "custom-provider API error (413): Payload Too Large",
        ),
        // Extension provider ids are not restricted to the old ASCII class:
        // `pi.registerProvider("my.provider", ...)` must classify too.
        (
            "my.provider",
            "my.provider API error (413): Payload Too Large",
        ),
        // Raw SDK error shape.
        (
            "openai",
            r#"413 {"error":{"message":"failed to parse request","code":""}}"#,
        ),
        // Bare status at end of string.
        ("openai", "413"),
        // Status followed by a colon.
        ("openai", "413: payload too large"),
        // Bodyless status for any provider (413 is provider-independent; only
        // the bodyless 400 stays Cerebras-scoped).
        ("opencode-go", "413 status code (no body)"),
    ] {
        let message = error_message(provider, text);
        assert!(
            is_context_overflow(&message, Some(1_000_000)),
            "expected overflow: {text}"
        );
        // Error-message detection does not need the context window.
        assert!(
            is_context_overflow(&message, None),
            "expected overflow: {text}"
        );
    }

    // Upstream's negative cases: other statuses, 413 in the body only, and
    // 413 as a prefix of a longer number.
    for (provider, text) in [
        (
            "github-copilot",
            r#"github-copilot API error (400): {"message":"failed to parse request","code":""}"#,
        ),
        (
            "github-copilot",
            r#"github-copilot API error (400): {"message":"failed to parse request: invalid value 413"}"#,
        ),
        (
            "custom-provider",
            "custom-provider API error (400): upstream mentioned OpenAI API error (413): invalid input",
        ),
        ("custom-provider", "custom-provider API error (4130): invalid input"),
        ("openai", "4130 failed to parse request"),
        // A 429 body is a rate limit even when it mentions a body limit.
        ("custom-provider", "custom-provider API error (429): Too many requests"),
    ] {
        let message = error_message(provider, text);
        assert!(
            !is_context_overflow(&message, Some(1_000_000)),
            "expected NOT overflow: {text}"
        );
    }

    // Only failed responses are checked.
    let mut not_an_error = error_message(
        "github-copilot",
        "github-copilot API error (413): failed to parse request",
    );
    not_an_error.stop_reason = StopReason::Stop;
    assert!(!is_context_overflow(&not_an_error, Some(1_000_000)));
    not_an_error.stop_reason = StopReason::Aborted;
    assert!(!is_context_overflow(&not_an_error, Some(1_000_000)));
}

#[tokio::test]
async fn d2_copilot_413_triggers_compact_and_retry() {
    let model = copilot_model();
    let copilot_413 =
        r#"github-copilot API error (413): {"message":"failed to parse request","code":""}"#;
    let terminal = error_message("github-copilot", copilot_413);

    // The overflow trigger fires for the failed Copilot turn.
    assert!(is_context_overflow(&terminal, Some(model.context_window)));

    // A same-bodied 400 is not request-size overflow, so no compaction is
    // attempted for it.
    let bad_request = error_message(
        "github-copilot",
        r#"github-copilot API error (400): {"message":"failed to parse request","code":""}"#,
    );
    assert!(!is_context_overflow(
        &bad_request,
        Some(model.context_window)
    ));

    // The overflow-recovery primitive drops the failed attempt, compacts and
    // hands back the context for one retry (mirrors
    // prompt_loop's in-memory compact-and-retry).
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message(&"large image request payload ".repeat(400)),
            stop_assistant("I can inspect the image."),
            user_message("continue"),
            Message::Assistant(terminal),
        ],
        tools: None,
    };
    let recovered = recover_from_overflow(
        context,
        copilot_413,
        &model,
        Arc::new(fake_complete("## Goal\n- continue after the 413")),
        &CompactionConfig {
            keep_recent_tokens: 1,
            ..CompactionConfig::default()
        },
    )
    .await
    .expect("the 413 classification must drive an in-memory compact-and-retry");

    // The failed 413 turn is gone; the compaction summary leads so the retry
    // request no longer carries the oversized history.
    assert!(recovered.messages.iter().all(|message| match message {
        Message::Assistant(assistant) => assistant.stop_reason != StopReason::Error,
        _ => true,
    }));
    match recovered.messages.first() {
        Some(Message::User(user)) => match &user.content[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.starts_with(COMPACTION_SUMMARY_PREFIX));
                assert!(text.text.contains("continue after the 413"));
            }
            other => panic!("expected text summary block, got {other:?}"),
        },
        other => panic!("expected summary user message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// #6 — per-request noRetryStatuses (upstream `classifier-shared.ts:61-102` /
// `provider-retry.ts:121`; OpenAI Decisions lists 504)
// ---------------------------------------------------------------------------

/// Scripted transport that answers every attempt with one status.
struct StatusOnlyTransport {
    status: u16,
    requests: Arc<AtomicUsize>,
}

impl HttpTransport for StatusOnlyTransport {
    fn execute(&self, _request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let status = self.status;
        Box::pin(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            // Keep a regression's backoff short so the control case stays fast.
            headers.insert(
                "retry-after-ms",
                reqwest::header::HeaderValue::from_static("1"),
            );
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(futures_util::stream::iter(vec![Ok(HttpBodyChunk::new(
                    b"gateway timeout".to_vec(),
                ))])),
            })
        })
    }
}

fn status_client(
    status: u16,
    max_retries: u32,
    requests: &Arc<AtomicUsize>,
) -> ProviderRequestClient {
    ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
            max_retries,
            max_retry_delay: Duration::from_secs(1),
            proxy_url: None,
            http_idle_timeout: None,
        },
        Arc::new(StatusOnlyTransport {
            status,
            requests: Arc::clone(requests),
        }),
    )
}

#[tokio::test]
async fn d2_no_retry_statuses_fail_a_504_after_one_request() {
    let requests = Arc::new(AtomicUsize::new(0));
    let client = status_client(504, 2, &requests);
    let mut chunks = Vec::new();
    let error = client
        .post_json_streaming_with_no_retry_statuses(
            "https://example.invalid/v1/decisions",
            &BTreeMap::new(),
            &serde_json::json!({"input": "oversized"}),
            None,
            &ProviderStreamHooks::default(),
            &[504],
            &mut |chunk: &[u8]| {
                chunks.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(&error, AiError::NonRetryable(_)), "{error:?}");
    assert!(error.to_string().contains("HTTP 504"), "{error}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a 504 listed in noRetryStatuses is sent exactly once"
    );
    assert!(
        chunks.is_empty(),
        "error bodies never reach the success chunk sink"
    );
}

#[tokio::test]
async fn d2_empty_no_retry_statuses_keeps_the_retry_policy() {
    // Control: the same status WITHOUT the per-request policy spends the
    // budget and reports the exhausted retries.
    let requests = Arc::new(AtomicUsize::new(0));
    let client = status_client(504, 1, &requests);
    let error = client
        .post_json_streaming_with_no_retry_statuses(
            "https://example.invalid/v1/decisions",
            &BTreeMap::new(),
            &serde_json::json!({"input": "oversized"}),
            None,
            &ProviderStreamHooks::default(),
            &[],
            &mut |_chunk: &[u8]| Ok(()),
        )
        .await
        .unwrap_err();
    // An exhausted retryable status still takes the ordinary non-2xx path, so
    // the error shape is the same as the no-retry case; only the request count
    // and the spent budget differ.
    assert!(matches!(&error, AiError::NonRetryable(_)), "{error:?}");
    assert!(error.to_string().contains("HTTP 504"), "{error}");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// P1-B (audit §5.1 #2) — the transport's REAL error output must drive the
// classifiers: overflow reachability, the upstream body cap, and the retry
// dispositions of the other statuses. The tests above feed handcrafted
// strings; these drive `ProviderRequestClient` with a scripted transport and
// feed the resulting `AiError` through the same finalizer seam the adapters
// use (`terminal_error_message` / `prefixed_terminal_error_message`).
// ---------------------------------------------------------------------------

/// Scripted transport that answers every attempt with one status and body.
struct BodyTransport {
    status: u16,
    body: Vec<u8>,
    requests: Arc<AtomicUsize>,
}

impl HttpTransport for BodyTransport {
    fn execute(&self, _request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let status = self.status;
        let body = self.body.clone();
        Box::pin(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            // Keep the retry path's backoff short for the control cases.
            headers.insert(
                "retry-after-ms",
                reqwest::header::HeaderValue::from_static("1"),
            );
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(futures_util::stream::iter(vec![Ok(HttpBodyChunk::new(
                    body,
                ))])),
            })
        })
    }
}

fn body_client(
    status: u16,
    body: impl Into<Vec<u8>>,
    max_retries: u32,
    requests: &Arc<AtomicUsize>,
) -> ProviderRequestClient {
    ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
            max_retries,
            max_retry_delay: Duration::from_secs(1),
            proxy_url: None,
            http_idle_timeout: None,
        },
        Arc::new(BodyTransport {
            status,
            body: body.into(),
            requests: Arc::clone(requests),
        }),
    )
}

/// Issue one request through the real transport loop and return its error.
async fn transport_error(client: &ProviderRequestClient) -> AiError {
    client
        .request_streaming(
            reqwest::Method::POST,
            "https://example.invalid/v1/responses",
            &BTreeMap::new(),
            b"{}".to_vec(),
            None,
            &ProviderStreamHooks::default(),
            &mut |_chunk: &[u8]| Ok(()),
        )
        .await
        .expect_err("a non-2xx response must fail the request")
}

#[tokio::test]
async fn d2_transport_413_reaches_overflow_classification_and_recovery() {
    use pir::ai::util::is_retryable_assistant_error;

    let body = r#"{"message":"failed to parse request","code":""}"#;
    let requests = Arc::new(AtomicUsize::new(0));
    let client = body_client(413, body.as_bytes().to_vec(), 0, &requests);
    let error = transport_error(&client).await;

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "413 is a request-size verdict, never a transport retry"
    );
    assert!(matches!(&error, AiError::NonRetryable(_)), "{error:?}");
    // Requirement 3: the AiError Display/type shape stays compatible.
    assert_eq!(
        error.to_string(),
        format!("non-retryable provider error: HTTP 413: {body}")
    );
    // Requirement 1: the status is also available structurally.
    let http = error
        .provider_http_error()
        .expect("the transport error carries a structured status");
    assert_eq!(http.status, 413);
    assert_eq!(http.body, body);

    let model = copilot_model();
    // The REAL Responses adapter seam: upstream's provider prefix shape.
    let terminal =
        pir::ai::api::prefixed_terminal_error_message(&model, &error, "OpenAI API error: ", None);
    assert_eq!(terminal.stop_reason, StopReason::Error);
    let text = terminal.error_message.clone().expect("error text");
    assert_eq!(text, format!("github-copilot API error (413): {body}"));
    assert!(
        is_context_overflow(&terminal, Some(model.context_window)),
        "the real transport text must classify as request-size overflow: {text}"
    );
    assert!(
        !is_retryable_assistant_error(&terminal),
        "a 413 takes compaction, not the turn retry"
    );

    // The unprefixed adapter seam keeps upstream's raw SDK shape.
    let raw = pir::ai::api::terminal_error_message(&model, &error, None);
    let expected_raw = format!("413: {body}");
    assert_eq!(raw.error_message.as_deref(), Some(expected_raw.as_str()));
    assert!(is_context_overflow(&raw, Some(model.context_window)));

    // The compact-and-retry primitive consumes the transport-built terminal.
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message(&"large image request payload ".repeat(400)),
            stop_assistant("I can inspect the image."),
            user_message("continue"),
            Message::Assistant(terminal),
        ],
        tools: None,
    };
    let recovered = recover_from_overflow(
        context,
        &text,
        &model,
        Arc::new(fake_complete("## Goal\n- continue after the transport 413")),
        &CompactionConfig {
            keep_recent_tokens: 1,
            ..CompactionConfig::default()
        },
    )
    .await
    .expect("the transport 413 must drive an in-memory compact-and-retry");
    assert!(recovered.messages.iter().all(|message| match message {
        Message::Assistant(assistant) => assistant.stop_reason != StopReason::Error,
        _ => true,
    }));
    match recovered.messages.first() {
        Some(Message::User(user)) => match &user.content[0] {
            ContentBlock::Text(text) => {
                assert!(text.text.starts_with(COMPACTION_SUMMARY_PREFIX));
                assert!(text.text.contains("continue after the transport 413"));
            }
            other => panic!("expected text summary block, got {other:?}"),
        },
        other => panic!("expected summary user message, got {other:?}"),
    }
}

#[tokio::test]
async fn d2_dotted_provider_413_is_request_size_overflow_end_to_end() {
    use pir::ai::util::is_retryable_assistant_error;

    // A real transport 413 + the real adapter finalizer for a provider id
    // containing a dot (`pi.registerProvider("my.provider", ...)`). The
    // overflow rule must see through the `<provider> API error (413):` prefix
    // instead of requiring an ASCII word-only provider name.
    let body = r#"{"message":"failed to parse request","code":""}"#;
    let requests = Arc::new(AtomicUsize::new(0));
    let client = body_client(413, body.as_bytes().to_vec(), 0, &requests);
    let error = transport_error(&client).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "413 never takes the transport retry"
    );

    let mut model = copilot_model();
    model.provider = "my.provider".into();
    let terminal =
        pir::ai::api::prefixed_terminal_error_message(&model, &error, "OpenAI API error: ", None);
    let text = terminal.error_message.clone().expect("error text");
    assert_eq!(text, format!("my.provider API error (413): {body}"));
    assert!(
        is_context_overflow(&terminal, Some(model.context_window)),
        "a dotted provider 413 must classify as request-size overflow: {text}"
    );
    assert!(
        !is_retryable_assistant_error(&terminal),
        "a 413 takes compaction, not the turn retry"
    );
}

#[tokio::test]
async fn d2_transport_500_and_429_keep_the_existing_retry_rules() {
    use pir::ai::util::is_retryable_assistant_error;

    for (status, body) in [(500u16, "overloaded"), (429u16, "rate limit exceeded")] {
        let requests = Arc::new(AtomicUsize::new(0));
        let client = body_client(status, body.as_bytes().to_vec(), 1, &requests);
        let error = transport_error(&client).await;

        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "{status} spends the transport retry budget (initial + 1)"
        );
        assert!(matches!(&error, AiError::NonRetryable(_)), "{error:?}");
        // Only the 413 text is reshaped; the exhausted statuses keep theirs.
        assert_eq!(
            error.to_string(),
            format!("non-retryable provider error: HTTP {status}: {body}")
        );
        let message = pir::ai::api::terminal_error_message(&copilot_model(), &error, None);
        assert_eq!(
            message.error_message.as_deref(),
            Some(format!("non-retryable provider error: HTTP {status}: {body}").as_str())
        );
        assert!(
            is_retryable_assistant_error(&message),
            "{status} stays retryable-looking for the classifiers"
        );
        assert!(
            !is_context_overflow(&message, Some(1_000_000)),
            "{status} is not a context overflow"
        );
    }
}

#[tokio::test]
async fn d2_transport_error_body_cap_is_4000_not_500() {
    // A tail keyword beyond the old 500-char cut survives into classification:
    // with the 500 cap this body was truncated before `request_too_large`.
    let mut body = "x".repeat(3500);
    body.push_str(" request_too_large");
    let requests = Arc::new(AtomicUsize::new(0));
    let client = body_client(400, body.clone().into_bytes(), 0, &requests);
    let error = transport_error(&client).await;
    assert_eq!(
        error.provider_http_error().expect("structured status").body,
        body,
        "a 3500-char body is surfaced in full"
    );
    let message = pir::ai::api::terminal_error_message(&copilot_model(), &error, None);
    assert!(
        is_context_overflow(&message, Some(1_000_000)),
        "the overflow keyword at offset 3500 must survive the 4000-char window"
    );

    // Longer bodies are cut at the upstream 4000-char window with upstream's
    // visible truncation marker (error-body.ts:18,139-142).
    let long = format!("{}{}", "y".repeat(6000), "TAIL_TOKEN");
    let requests = Arc::new(AtomicUsize::new(0));
    let client = body_client(400, long.clone().into_bytes(), 0, &requests);
    let error = transport_error(&client).await;
    let cap = pir::ai::transport::MAX_PROVIDER_ERROR_BODY_CHARS;
    assert_eq!(cap, 4000, "upstream MAX_PROVIDER_ERROR_BODY_CHARS");
    let expected = format!(
        "{}... [truncated {} chars]",
        "y".repeat(cap),
        long.chars().count() - cap
    );
    assert_eq!(
        error.provider_http_error().expect("structured status").body,
        expected
    );
    assert!(!error.to_string().contains("TAIL_TOKEN"), "{error}");
}

#[tokio::test]
async fn d2_transport_401_and_403_stay_non_retryable() {
    use pir::ai::util::is_retryable_assistant_error;

    for status in [401u16, 403] {
        let requests = Arc::new(AtomicUsize::new(0));
        let client = body_client(
            status,
            br#"{"error":"invalid token"}"#.to_vec(),
            2,
            &requests,
        );
        let error = transport_error(&client).await;

        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "{status} is permanent even with a retry budget"
        );
        assert!(matches!(&error, AiError::NonRetryable(_)), "{error:?}");
        let message = pir::ai::api::terminal_error_message(&copilot_model(), &error, None);
        assert!(
            !is_retryable_assistant_error(&message),
            "{status} must never look transient"
        );
        assert!(
            !is_context_overflow(&message, Some(1_000_000)),
            "{status} is not an overflow"
        );
    }

    // Adapter-authored NonRetryable text is not mistaken for an HTTP failure.
    assert!(
        AiError::NonRetryable("invalid request: unknown field".into())
            .provider_http_error()
            .is_none()
    );
}
