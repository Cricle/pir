use super::*;

// ---------------------------------------------------------------------------
// System One (upstream `system-one-shared.ts` + its two transports)
// ---------------------------------------------------------------------------

/// Differences between services that serve System One models (upstream
/// `SystemOneTransport`).
pub(super) struct SystemOneTransport {
    /// Service name used in error messages.
    label: &'static str,
}

impl SystemOneTransport {
    /// TypeSafe's native System One protocol (upstream
    /// `typesafe-system-one.ts`); OpenRouter serves the same protocol with a
    /// different base URL.
    pub(super) fn typesafe() -> Self {
        Self {
            label: "System One API",
        }
    }

    /// System One on the Cloudflare Workers AI REST endpoint (upstream
    /// `cloudflare-workers-ai-system-one.ts`).
    pub(super) fn cloudflare() -> Self {
        Self {
            label: "Cloudflare Workers AI",
        }
    }

    /// The absolute request URL (upstream `transport.url`).
    fn url(&self, model: &ClassifierModel) -> String {
        let base = model.base_url.trim_end_matches('/');
        match model.api {
            ClassifierApi::TypesafeSystemOne => format!("{base}/systemone"),
            ClassifierApi::CloudflareWorkersAiSystemOne => {
                // Resolve the `{CLOUDFLARE_ACCOUNT_ID}` endpoint placeholder
                // from the environment (upstream `resolveCloudflareModel`).
                let mut env = BTreeMap::new();
                for name in [
                    crate::ai::auth::cloudflare::ACCOUNT_ID,
                    crate::ai::auth::cloudflare::GATEWAY_ID,
                ] {
                    if let Ok(value) = std::env::var(name) {
                        env.insert(name.to_owned(), value);
                    }
                }
                crate::ai::auth::resolve_cloudflare_base_url(&format!("{base}/run"), &env)
            }
            ClassifierApi::OpenAiDecisions => not_a_system_one_shape(),
        }
    }

    /// Wraps the System One request in the service's request envelope
    /// (upstream `transport.payload`).
    fn payload(&self, model: &ClassifierModel, request: &JsonValue) -> JsonValue {
        match model.api {
            ClassifierApi::TypesafeSystemOne => {
                let mut body = serde_json::Map::new();
                body.insert("model".to_owned(), JsonValue::String(model.id.clone()));
                if let JsonValue::Object(fields) = request {
                    for (key, value) in fields {
                        body.insert(key.clone(), value.clone());
                    }
                }
                JsonValue::Object(body)
            }
            ClassifierApi::CloudflareWorkersAiSystemOne => serde_json::json!({
                "model": model.id,
                "input": request,
            }),
            ClassifierApi::OpenAiDecisions => JsonValue::Null,
        }
    }

    /// Extracts the System One output (`{ answers, usage }`) from the
    /// service's response envelope (upstream `transport.output`).
    fn output(&self, model: &ClassifierModel, body: &JsonValue) -> AiResult<JsonValue> {
        match model.api {
            ClassifierApi::TypesafeSystemOne => {
                if body.is_object() {
                    Ok(body.clone())
                } else {
                    Err(unexpected(self.label))
                }
            }
            ClassifierApi::CloudflareWorkersAiSystemOne => {
                if !body.is_object() {
                    return Err(unexpected(self.label));
                }
                if body.get("success").and_then(JsonValue::as_bool) == Some(false) {
                    let messages = body
                        .get("errors")
                        .and_then(JsonValue::as_array)
                        .map(|errors| {
                            errors
                                .iter()
                                .filter_map(|error| {
                                    error.get("message").and_then(JsonValue::as_str)
                                })
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    return Err(AiError::NonRetryable(format!(
                        "{} error: {messages}",
                        self.label
                    )));
                }
                let result = body.get("result").ok_or_else(|| unexpected(self.label))?;
                if !result.is_object() {
                    return Err(unexpected(self.label));
                }
                if result.get("answers").is_some() {
                    return Ok(result.clone());
                }
                if result.get("state").and_then(JsonValue::as_str) != Some("Completed") {
                    return Err(AiError::NonRetryable(format!(
                        "{} run did not complete (state: {})",
                        self.label,
                        result
                            .get("state")
                            .map(JsonValue::to_string)
                            .unwrap_or_else(|| "unknown".to_owned())
                    )));
                }
                result
                    .get("result")
                    .filter(|inner| inner.is_object())
                    .cloned()
                    .ok_or_else(|| unexpected(self.label))
            }
            ClassifierApi::OpenAiDecisions => Err(unexpected("OpenAI Decisions")),
        }
    }
}

/// Stand-in for upstream's impossible-transport arm: the enum dispatch above
/// guarantees this never runs, and returning a typed error keeps the function
/// total without `unreachable!`.
pub(super) fn unexpected(label: &str) -> AiError {
    AiError::NonRetryable(format!("{label} returned an unexpected response"))
}

/// Stand-in for the Decisions arm inside the System One transport URL
/// builder: the dispatch in [`classify`] routes each api to exactly one
/// transport, so this arm is unreachable by construction; returning an empty
/// value keeps the match total without `unreachable!`.
fn not_a_system_one_shape() -> String {
    String::new()
}

// ---------------------------------------------------------------------------
// Image input (upstream `system-one-shared.ts` `addImages` + transport
// `detectCompat`)
// ---------------------------------------------------------------------------

use crate::ai::types::ImageContent;

/// TypeSafe's opt-in header for its experimental image input.
const IMAGE_OPT_IN_HEADER: &str = "TypeSafe-Danger-Zone-Images";
/// Clef accepts at most this many data-URL images per request.
const MAX_DATA_URL_IMAGES: usize = 4;

/// How a service takes images (upstream `SystemOneImageInput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemOneImageInput {
    /// TypeSafe's `assets` map, referenced from the state.
    Assets,
    /// Clef's `images` list of data URLs.
    Images,
    /// OpenRouter's `{type:"image_url"}` parts placed before the state.
    StateImageUrls,
}

impl SystemOneTransport {
    /// Auto-detected image form (upstream `transport.detectCompat`).
    fn detect_image_input(&self, model: &ClassifierModel) -> SystemOneImageInput {
        match model.api {
            ClassifierApi::TypesafeSystemOne => {
                if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
                    SystemOneImageInput::StateImageUrls
                } else {
                    SystemOneImageInput::Assets
                }
            }
            ClassifierApi::CloudflareWorkersAiSystemOne => {
                if model.id.starts_with("@cf/cloudflare/clef") {
                    SystemOneImageInput::Images
                } else {
                    SystemOneImageInput::Assets
                }
            }
            ClassifierApi::OpenAiDecisions => SystemOneImageInput::Assets,
        }
    }

    /// Add the context images to `request` in the service's form (upstream
    /// `addImages`). Only PNG/JPEG (or WebP for Clef's list) are accepted;
    /// the errors are non-retryable, like upstream's thrown `Error`s.
    fn add_images(
        &self,
        model: &ClassifierModel,
        input: SystemOneImageInput,
        images: &[ImageContent],
        request: &mut JsonValue,
    ) -> AiResult<()> {
        let label = self.label;
        match input {
            SystemOneImageInput::Images => {
                if images.len() > MAX_DATA_URL_IMAGES {
                    return Err(AiError::NonRetryable(format!(
                        "{label} accepts at most {MAX_DATA_URL_IMAGES} images for {}, got {}",
                        model.id,
                        images.len()
                    )));
                }
                let mut urls = Vec::with_capacity(images.len());
                for image in images {
                    if !matches!(
                        image.mime_type.as_str(),
                        "image/png" | "image/jpeg" | "image/webp"
                    ) {
                        return Err(AiError::NonRetryable(format!(
                            "{label} accepts PNG, JPEG, and WebP images for {}, got {}",
                            model.id, image.mime_type
                        )));
                    }
                    urls.push(JsonValue::String(format!(
                        "data:{};base64,{}",
                        image.mime_type, image.data
                    )));
                }
                if let Some(object) = request.as_object_mut() {
                    object.insert("images".to_owned(), JsonValue::Array(urls));
                }
            }
            SystemOneImageInput::StateImageUrls => {
                let parts = images
                    .iter()
                    .map(|image| {
                        JsonValue::Object(
                            [
                                ("type".to_owned(), JsonValue::String("image_url".to_owned())),
                                (
                                    "image_url".to_owned(),
                                    JsonValue::Object(
                                        [(
                                            "url".to_owned(),
                                            JsonValue::String(format!(
                                                "data:{};base64,{}",
                                                image.mime_type, image.data
                                            )),
                                        )]
                                        .into_iter()
                                        .collect(),
                                    ),
                                ),
                            ]
                            .into_iter()
                            .collect(),
                        )
                    })
                    .collect::<Vec<_>>();
                let object = request.as_object_mut().ok_or_else(|| unexpected(label))?;
                let original_state = object.remove("state").unwrap_or(JsonValue::Null);
                let mut state = parts;
                state.push(original_state);
                object.insert("state".to_owned(), JsonValue::Array(state));
            }
            SystemOneImageInput::Assets => {
                let mut assets = serde_json::Map::new();
                let mut names = Vec::with_capacity(images.len());
                for (index, image) in images.iter().enumerate() {
                    let format = match image.mime_type.as_str() {
                        "image/png" => "png",
                        "image/jpeg" => "jpeg",
                        _ => {
                            return Err(AiError::NonRetryable(format!(
                                "{label} accepts PNG and JPEG images for {}, got {}",
                                model.id, image.mime_type
                            )));
                        }
                    };
                    let name = format!("<image-{}>", index + 1);
                    assets.insert(
                        name.clone(),
                        JsonValue::Object(
                            [
                                ("type".to_owned(), JsonValue::String("image".to_owned())),
                                ("format".to_owned(), JsonValue::String(format.to_owned())),
                                (
                                    "data_base64".to_owned(),
                                    JsonValue::String(image.data.clone()),
                                ),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    );
                    names.push(JsonValue::String(name));
                }
                let object = request.as_object_mut().ok_or_else(|| unexpected(label))?;
                object.insert("assets".to_owned(), JsonValue::Object(assets));
                let original_state = object.remove("state").unwrap_or(JsonValue::Null);
                let mut state = names;
                state.push(original_state);
                object.insert("state".to_owned(), JsonValue::Array(state));
            }
        }
        Ok(())
    }
}

/// Whether requests with images for this model send TypeSafe's opt-in header
/// (upstream `sendImageOptInHeader`; pir auto-detects because the classifier
/// model shape carries no `compat` block).
fn image_opt_in_header_for(model: &ClassifierModel) -> bool {
    match model.api {
        ClassifierApi::TypesafeSystemOne => {
            !(model.provider == "openrouter" || model.base_url.contains("openrouter.ai"))
        }
        ClassifierApi::CloudflareWorkersAiSystemOne | ClassifierApi::OpenAiDecisions => false,
    }
}

/// Map the public `bool` questions to TypeSafe's wire-level `noul` type and
/// collect the request body (upstream `wireRequest`). Public for tests/.
pub fn system_one_wire_request(context: &ClassifierContext) -> JsonValue {
    let questions: serde_json::Map<String, JsonValue> = context
        .questions
        .iter()
        .map(|(id, question)| {
            let mut value = serde_json::to_value(question).unwrap_or(JsonValue::Null);
            if question_is_bool(question) {
                if let Some(fields) = value.as_object_mut() {
                    fields.insert("type".to_owned(), JsonValue::String("noul".to_owned()));
                }
            }
            (id.clone(), value)
        })
        .collect();
    serde_json::json!({ "state": context.state, "questions": questions })
}

fn question_is_bool(question: &ClassifierQuestion) -> bool {
    matches!(question, ClassifierQuestion::Bool { .. })
}

/// Parse the System One answers map against the requested questions
/// (upstream `parseAnswers`): bool answers arrive as wire-level `noul`.
/// Public for tests/.
pub fn parse_system_one_answers(
    label: &str,
    value: &JsonValue,
    context: &ClassifierContext,
) -> AiResult<BTreeMap<String, ClassifierAnswer>> {
    let answers = value.as_object().ok_or_else(|| unexpected(label))?;
    let mut parsed = BTreeMap::new();
    for (id, question) in &context.questions {
        let answer = answers.get(id).ok_or_else(|| {
            AiError::NonRetryable(format!("{label} did not return an answer for {id}"))
        })?;
        let answer_type = answer.get("type").and_then(JsonValue::as_str);
        let required_number = |field: &str| -> AiResult<f64> {
            answer
                .get(field)
                .and_then(JsonValue::as_f64)
                .filter(|value| value.is_finite())
                .ok_or_else(|| {
                    AiError::NonRetryable(format!("{label} returned an invalid {field} for {id}"))
                })
        };
        match question {
            ClassifierQuestion::Choice { .. } => {
                if answer_type != Some("choice") {
                    return Err(AiError::NonRetryable(format!(
                        "{label} did not return a choice answer for {id}"
                    )));
                }
                let choice = answer
                    .get("choice")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| {
                        AiError::NonRetryable(format!(
                            "{label} did not return a choice answer for {id}"
                        ))
                    })?
                    .to_owned();
                let probabilities = answer
                    .get("probabilities")
                    .and_then(JsonValue::as_object)
                    .ok_or_else(|| {
                        AiError::NonRetryable(format!(
                            "{label} returned invalid probabilities for {id}"
                        ))
                    })?
                    .iter()
                    .map(|(key, probability)| {
                        probability
                            .as_f64()
                            .filter(|value| value.is_finite())
                            .map(|probability| (key.clone(), probability))
                    })
                    .collect::<Option<BTreeMap<String, f64>>>()
                    .ok_or_else(|| {
                        AiError::NonRetryable(format!(
                            "{label} returned invalid probabilities for {id}"
                        ))
                    })?;
                let confidence = required_number("confidence")?;
                parsed.insert(
                    id.clone(),
                    ClassifierAnswer::Choice {
                        choice,
                        probabilities,
                        confidence,
                    },
                );
            }
            ClassifierQuestion::Score { .. } => {
                if answer_type != Some("score") {
                    return Err(AiError::NonRetryable(format!(
                        "{label} did not return a score answer for {id}"
                    )));
                }
                let score = required_number("score")?;
                let confidence = required_number("confidence")?;
                parsed.insert(id.clone(), ClassifierAnswer::Score { score, confidence });
            }
            ClassifierQuestion::Bool { .. } => {
                if answer_type != Some("noul") {
                    return Err(AiError::NonRetryable(format!(
                        "{label} did not return a bool answer for {id}"
                    )));
                }
                let probability = required_number("noul")?;
                parsed.insert(id.clone(), ClassifierAnswer::Bool { probability });
            }
        }
    }
    Ok(parsed)
}

/// One System One classification over the given transport (upstream
/// `classifySystemOne`).
pub(super) async fn classify_system_one(
    transport: &SystemOneTransport,
    client: &ProviderRequestClient,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
    signal: Option<&crate::ai::types::AbortSignal>,
) -> ClassifierResult {
    let mut result = empty_result(model);
    // Image-input conversion (upstream `classifySystemOne`): a context with
    // images is converted into the service's image form before the payload
    // is wrapped; unsupported image models return an error result.
    let mut request = system_one_wire_request(context);
    let mut image_opt_in_header = false;
    if !context.images.is_empty() {
        if !model.accepts_image_input() {
            return error_result(
                &mut result,
                aborted_now(signal),
                format!(
                    "{} does not support image input for {}",
                    transport.label, model.id
                ),
            );
        }
        let input = transport.detect_image_input(model);
        if let Err(error) = transport.add_images(model, input, &context.images, &mut request) {
            return error_result(
                &mut result,
                aborted_now(signal),
                format!("{} error: {}", transport.label, error.user_message()),
            );
        }
        image_opt_in_header = image_opt_in_header_for(model);
    }
    let Some(api_key) = crate::ai::images::resolve_api_key_for_classifier(&model.provider, options)
        .filter(|key| !key.is_empty())
    else {
        // Upstream treats an empty key as missing (`if (!options?.apiKey)`).
        return error_result(
            &mut result,
            aborted_now(signal),
            format!("No API key for provider: {}", model.provider),
        );
    };
    let url = transport.url(model);
    let body = transport.payload(model, &request);
    let opt_in_headers: Vec<(&str, &str)> = if image_opt_in_header {
        vec![(IMAGE_OPT_IN_HEADER, "true")]
    } else {
        Vec::new()
    };
    let headers = request_headers(model, &api_key, options, &opt_in_headers);
    let response = match post_classifier_request(client, &url, headers, &body, signal).await {
        Ok(response) => response,
        Err(error) => {
            return error_result(
                &mut result,
                matches!(error, AiError::Aborted) || aborted_now(signal),
                format!("{} error: {}", transport.label, error.user_message()),
            );
        }
    };
    // Set before parsing answers: a request with malformed answers was still
    // billed (upstream comment).
    result.usage = parse_classifier_usage(response.get("usage"), model);
    let output = match transport.output(model, &response) {
        Ok(output) => output,
        Err(error) => {
            return error_result(
                &mut result,
                aborted_now(signal),
                format!("{} error: {}", transport.label, error.user_message()),
            );
        }
    };
    match parse_system_one_answers(
        transport.label,
        output.get("answers").unwrap_or(&JsonValue::Null),
        context,
    ) {
        Ok(answers) => {
            result.answers = answers;
            result
        }
        Err(error) => error_result(
            &mut result,
            aborted_now(signal),
            format!("{} error: {}", transport.label, error.user_message()),
        ),
    }
}
