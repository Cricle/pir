//! Structured-classifier APIs (upstream `packages/ai/src/api/`:
//! `classifier-shared.ts`, `system-one-shared.ts`, `typesafe-system-one.ts`,
//! `cloudflare-workers-ai-system-one.ts`, `openai-decisions.ts`).
//!
//! Dispatch happens on the typed [`ClassifierApi`] enum carried by
//! [`ClassifierModel`]: the upstream per-provider `classifiers` registry
//! maps become exhaustive `match` arms. Result semantics mirror upstream:
//! failures return [`ClassifierResult`] with `stop_reason: Error|Aborted`
//! and `error_message`, while every internal step is a typed `AiResult`.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::ai::error::{AiError, AiResult};
use crate::ai::images::unix_millis;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::{
    ClassifierAnswer, ClassifierApi, ClassifierBoolCriteria, ClassifierContext, ClassifierModel,
    ClassifierOptions, ClassifierQuestion, ClassifierRequestOptions, ClassifierResult,
    ClassifierStopReason, Cost, Usage,
};

/// Dispatch one classification on the model's typed `api` (upstream
/// `Models.classify` + the per-provider classifier registries). The
/// image-input gate mirrors `Models.classify`: models whose `input` does not
/// include `image` reject a context carrying images before any request.
pub async fn classify(
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
    signal: Option<&crate::ai::types::AbortSignal>,
) -> ClassifierResult {
    let mut extended = ClassifierRequestOptions::from(options);
    extended.signal = signal.cloned();
    classify_with(model, context, &extended).await
}

/// [`classify`] with the extended request surface (upstream
/// `ClassifierOptions` extending `ProviderRequestOptions`): cancellation,
/// provider-scoped env overrides, and the timeout/retry budget ride on the
/// options instead of separate arguments.
pub async fn classify_with(
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierRequestOptions,
) -> ClassifierResult {
    let signal = options.signal.as_ref();
    if !context.images.is_empty() && !model.accepts_image_input() {
        let mut result = empty_result(model);
        return error_result(
            &mut result,
            false,
            format!(
                "Model {}/{} does not accept image input",
                model.provider, model.id
            ),
        );
    }
    // Resolve auth once with request-env precedence, then hand the resolved
    // key to the per-API classifiers (upstream merges provider-resolved auth
    // with explicit options before dispatch).
    let base = ClassifierOptions {
        api_key: crate::ai::images::resolve_classifier_api_key_with_env(
            &model.provider,
            options.api_key.as_deref(),
            options.env.as_ref(),
        ),
        headers: options.headers.clone(),
        temperature: options.temperature,
    };
    let client = match crate::ai::images::request_client(
        options.timeout_ms,
        options.max_retries,
        options.max_retry_delay_ms,
    ) {
        Ok(client) => client,
        Err(error) => {
            let mut result = empty_result(model);
            return error_result(&mut result, aborted_now(signal), error.user_message());
        }
    };
    match model.api {
        ClassifierApi::TypesafeSystemOne => {
            classify_system_one(
                &SystemOneTransport::typesafe(),
                &client,
                model,
                context,
                &base,
                signal,
            )
            .await
        }
        ClassifierApi::CloudflareWorkersAiSystemOne => {
            classify_system_one(
                &SystemOneTransport::cloudflare(),
                &client,
                model,
                context,
                &base,
                signal,
            )
            .await
        }
        ClassifierApi::OpenAiDecisions => {
            classify_openai_decisions(&client, model, context, &base, signal).await
        }
    }
}

fn empty_result(model: &ClassifierModel) -> ClassifierResult {
    ClassifierResult {
        api: model.api,
        provider: model.provider.clone(),
        model: model.id.clone(),
        answers: BTreeMap::new(),
        usage: None,
        stop_reason: ClassifierStopReason::Stop,
        error_message: None,
        timestamp: unix_millis(),
    }
}

fn error_result(result: &mut ClassifierResult, aborted: bool, message: String) -> ClassifierResult {
    result.stop_reason = if aborted {
        ClassifierStopReason::Aborted
    } else {
        ClassifierStopReason::Error
    };
    result.error_message = Some(message);
    result.clone()
}

fn aborted_now(signal: Option<&crate::ai::types::AbortSignal>) -> bool {
    signal.is_some_and(|signal| signal.is_aborted())
}

// ---------------------------------------------------------------------------
// Shared request execution (upstream `classifier-shared.ts`)
// ---------------------------------------------------------------------------

/// POST one JSON classifier request with bearer auth and transport-level
/// retries (upstream `postClassifierRequest`; the retry/timeout budget comes
/// from pir's shared `ProviderRequestClient` defaults). Returns the parsed
/// response body.
async fn post_classifier_request(
    client: &ProviderRequestClient,
    url: &str,
    headers: BTreeMap<String, String>,
    body: &JsonValue,
    signal: Option<&crate::ai::types::AbortSignal>,
) -> AiResult<JsonValue> {
    post_classifier_request_with_no_retry(client, url, headers, body, signal, &[]).await
}

/// [`post_classifier_request`] with upstream `noRetryStatuses`
/// (`classifier-shared.ts:61-102`): listed HTTP statuses fail at once although
/// the shared transport policy would normally retry them.
async fn post_classifier_request_with_no_retry(
    client: &ProviderRequestClient,
    url: &str,
    headers: BTreeMap<String, String>,
    body: &JsonValue,
    signal: Option<&crate::ai::types::AbortSignal>,
    no_retry_statuses: &[u16],
) -> AiResult<JsonValue> {
    let mut received = Vec::new();
    let (_, _) = client
        .post_json_streaming_with_no_retry_statuses(
            url,
            &headers,
            body,
            signal,
            // Utility HTTP (not an AI provider request): no transport hooks.
            &crate::ai::types::ProviderStreamHooks::default(),
            no_retry_statuses,
            &mut |chunk| {
                received.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await?;
    serde_json::from_slice(&received)
        .map_err(|error| AiError::NonRetryable(format!("unreadable response body: {error}")))
}

/// Build the shared bearer + JSON headers from the model's defaults.
///
/// `defaults` are transport-derived headers (e.g. TypeSafe's image opt-in)
/// applied over the bearer/JSON pair but under the model and caller headers,
/// matching upstream's `{...defaults, ...options?.headers}` merge order.
fn request_headers(
    model: &ClassifierModel,
    api_key: &str,
    options: &ClassifierOptions,
    defaults: &[(&str, &str)],
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_owned(), format!("Bearer {api_key}"));
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    for (name, value) in defaults {
        headers.insert((*name).to_owned(), (*value).to_owned());
    }
    for (name, value) in model.headers.clone().unwrap_or_default() {
        headers.insert(name.clone(), value.clone());
    }
    // Caller headers override model defaults (upstream merges last).
    for (name, value) in &options.headers {
        headers.insert(name.clone(), value.clone());
    }
    headers
}

/// Usage from a `{ input_tokens, output_tokens }` object, priced from the
/// model's catalog rates (upstream `parseClassifierUsage`). A missing or
/// malformed usage object yields `None` instead of failing the result.
pub(crate) fn parse_classifier_usage(
    value: Option<&JsonValue>,
    model: &ClassifierModel,
) -> Option<Usage> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    let input = value
        .get("input_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let has_usage = value.get("input_tokens").is_some() || value.get("output_tokens").is_some();
    if !has_usage {
        return None;
    }
    let mut usage = Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output,
        cost: Cost::default(),
    };
    usage.cost.input = model.cost.rates.input / 1_000_000.0 * input as f64;
    usage.cost.output = model.cost.rates.output / 1_000_000.0 * output as f64;
    usage.cost.total = usage.cost.input + usage.cost.output;
    usage.cost.total_from_provider = Some(false);
    Some(usage)
}
mod decisions;
mod system_one;

use decisions::classify_openai_decisions;
pub use decisions::{decisions_wire_input, decisions_wire_question, parse_decisions_answers};
use system_one::{classify_system_one, unexpected, SystemOneTransport};
pub use system_one::{parse_system_one_answers, system_one_wire_request};
