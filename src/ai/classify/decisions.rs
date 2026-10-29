use super::*;

// ---------------------------------------------------------------------------
// OpenAI Decisions (upstream `openai-decisions.ts`)
// ---------------------------------------------------------------------------

/// The endpoint accepts at most this many image parts per request (upstream
/// `MAX_IMAGES`).
const DECISIONS_MAX_IMAGES: usize = 128;

/// Statuses that fail at once although the shared transport retries them
/// (upstream `openai-decisions.ts:147-151`). Cloudflare in front of
/// `api.openai.com` answers 504 with an HTML page when a request runs longer
/// than its edge time limit; retrying the same oversized input just runs into
/// the same gateway timeout.
const DECISIONS_NO_RETRY_STATUSES: [u16; 1] = [504];

/// Build one Decisions question (upstream `wireQuestion`): predicates have no
/// criteria field, so the meanings of true and false are appended to the
/// instructions. Public for tests/.
pub fn decisions_wire_question(id: &str, question: &ClassifierQuestion) -> JsonValue {
    match question {
        ClassifierQuestion::Choice {
            instructions,
            criteria,
        } => serde_json::json!({
            "type": "choice",
            "name": id,
            "instructions": instructions,
            "choices": criteria
                .iter()
                .map(|(value, description)| if description.is_empty() {
                    serde_json::json!({ "value": value })
                } else {
                    serde_json::json!({ "value": value, "description": description })
                })
                .collect::<Vec<_>>(),
        }),
        ClassifierQuestion::Score {
            instructions,
            criteria,
        } => serde_json::json!({
            "type": "score",
            "name": id,
            "instructions": instructions,
            "levels": criteria
                .iter()
                .map(|label| serde_json::json!({ "label": label }))
                .collect::<Vec<_>>(),
        }),
        ClassifierQuestion::Bool {
            instructions,
            criteria:
                ClassifierBoolCriteria {
                    true_meaning,
                    false_meaning,
                },
        } => {
            let mut instructions = instructions.clone();
            let mut meanings = Vec::new();
            if !true_meaning.is_empty() {
                meanings.push(format!("True means: {true_meaning}"));
            }
            if !false_meaning.is_empty() {
                meanings.push(format!("False means: {false_meaning}"));
            }
            for meaning in meanings {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(&meaning);
            }
            serde_json::json!({
                "type": "predicate",
                "name": id,
                "instructions": instructions,
            })
        }
    }
}

/// Build the Decisions request input (upstream `wireInput`): the state as
/// JSON text, or one user message with `input_text` + `input_image` data URLs.
/// Public for tests/.
pub fn decisions_wire_input(context: &ClassifierContext) -> AiResult<JsonValue> {
    let state = serde_json::to_string(&context.state).unwrap_or_else(|_| "{}".to_owned());
    if context.images.is_empty() {
        return Ok(JsonValue::String(state));
    }
    if context.images.len() > DECISIONS_MAX_IMAGES {
        return Err(AiError::NonRetryable(format!(
            "OpenAI Decisions accepts at most {DECISIONS_MAX_IMAGES} images, got {}",
            context.images.len()
        )));
    }
    let mut content = vec![serde_json::json!({ "type": "input_text", "text": state })];
    for image in &context.images {
        content.push(serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
        }));
    }
    Ok(serde_json::json!([{ "role": "user", "content": content }]))
}

/// Parse the Decisions answers array against the requested questions
/// (upstream `parseAnswers`). Public for tests/.
pub fn parse_decisions_answers(
    value: &JsonValue,
    context: &ClassifierContext,
) -> AiResult<BTreeMap<String, ClassifierAnswer>> {
    let by_name: BTreeMap<&str, &JsonValue> = value
        .as_array()
        .ok_or_else(|| unexpected("OpenAI Decisions"))?
        .iter()
        .filter_map(|answer| {
            let name = answer.get("name").and_then(JsonValue::as_str)?;
            Some((name, answer))
        })
        .collect();
    let label = "OpenAI Decisions";
    let mut parsed = BTreeMap::new();
    for (id, question) in &context.questions {
        let answer = by_name.get(id.as_str()).copied().ok_or_else(|| {
            AiError::NonRetryable(format!("{label} did not return an answer for {id}"))
        })?;
        let answer_type = answer.get("type").and_then(JsonValue::as_str);
        if answer_type == Some("refusal") {
            return Err(AiError::NonRetryable(format!(
                "{label} refused to answer {id}"
            )));
        }
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
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| {
                        AiError::NonRetryable(format!(
                            "{label} returned invalid probabilities for {id}"
                        ))
                    })?
                    .iter()
                    .map(|entry| {
                        let key = entry.get("value").and_then(JsonValue::as_str)?;
                        let probability = entry
                            .get("probability")
                            .and_then(JsonValue::as_f64)
                            .filter(|value| value.is_finite())?;
                        Some((key.to_owned(), probability))
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
                if answer_type != Some("predicate") {
                    return Err(AiError::NonRetryable(format!(
                        "{label} did not return a predicate answer for {id}"
                    )));
                }
                let probability = required_number("probability")?;
                parsed.insert(id.clone(), ClassifierAnswer::Bool { probability });
            }
        }
    }
    Ok(parsed)
}

/// One classification through OpenAI's Decisions API (upstream
/// `openai-decisions.ts`).
pub(super) async fn classify_openai_decisions(
    client: &ProviderRequestClient,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
    signal: Option<&crate::ai::types::AbortSignal>,
) -> ClassifierResult {
    let mut result = empty_result(model);
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
    let body = serde_json::json!({
        "model": model.id,
        "input": match decisions_wire_input(context) {
            Ok(input) => input,
            Err(error) => {
                return error_result(
                    &mut result,
                    aborted_now(signal),
                    format!("OpenAI Decisions error: {}", error.user_message()),
                );
            }
        },
        "questions": context
            .questions
            .iter()
            .map(|(id, question)| decisions_wire_question(id, question))
            .collect::<Vec<_>>(),
    });
    let url = format!("{}/decisions", model.base_url.trim_end_matches('/'));
    let headers = request_headers(model, &api_key, options, &[]);
    let response = match post_classifier_request_with_no_retry(
        client,
        &url,
        headers,
        &body,
        signal,
        &DECISIONS_NO_RETRY_STATUSES,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return error_result(
                &mut result,
                matches!(error, AiError::Aborted) || aborted_now(signal),
                format!("OpenAI Decisions error: {}", error.user_message()),
            );
        }
    };
    // Set before parsing answers: a request with malformed or refused answers
    // was still billed (upstream comment).
    result.usage = parse_classifier_usage(response.get("usage"), model);
    match parse_decisions_answers(response.get("answers").unwrap_or(&JsonValue::Null), context) {
        Ok(answers) => {
            result.answers = answers;
            result
        }
        Err(error) => error_result(
            &mut result,
            aborted_now(signal),
            format!("OpenAI Decisions error: {}", error.user_message()),
        ),
    }
}
