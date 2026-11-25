//! The answer-validation loop shared by the Decisions and System One
//! classifiers (upstream `openai-decisions.ts` / `system-one-shared.ts`
//! `parseAnswers`). The wire shapes differ only in how answers and choice
//! probabilities are keyed, so the checks and error text live here once.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{ClassifierAnswer, ClassifierContext, ClassifierQuestion};

/// How the provider encodes choice probabilities.
pub(super) enum ChoiceProbabilities {
    /// Decisions: `[{ "value": "...", "probability": 0.7 }]`.
    Entries,
    /// System One: `{ "value": 0.7 }`.
    Map,
}

/// Validate every requested answer and convert it into [`ClassifierAnswer`].
///
/// `get_answer` resolves one question id to its wire object. `refusal`
/// enables the Decisions refusal branch; `bool_type`/`bool_field` name the
/// wire's bool answer discriminator and probability field (`predicate` /
/// `probability` for Decisions, `noul` / `noul` for System One).
pub(super) fn parse_classifier_answers<'a>(
    label: &str,
    context: &ClassifierContext,
    get_answer: impl Fn(&str) -> Option<&'a JsonValue>,
    probabilities: ChoiceProbabilities,
    refusal: bool,
    bool_type: &str,
    bool_field: &str,
) -> AiResult<BTreeMap<String, ClassifierAnswer>> {
    let mut parsed = BTreeMap::new();
    for (id, question) in &context.questions {
        let answer = get_answer(id).ok_or_else(|| {
            AiError::NonRetryable(format!("{label} did not return an answer for {id}"))
        })?;
        let answer_type = answer.get("type").and_then(JsonValue::as_str);
        if refusal && answer_type == Some("refusal") {
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
                let raw_probabilities = answer.get("probabilities");
                let probabilities = match probabilities {
                    ChoiceProbabilities::Entries => raw_probabilities
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| invalid_probabilities(label, id))?
                        .iter()
                        .map(|entry| {
                            let key = entry.get("value").and_then(JsonValue::as_str)?;
                            let probability = entry
                                .get("probability")
                                .and_then(JsonValue::as_f64)
                                .filter(|value| value.is_finite())?;
                            Some((key.to_owned(), probability))
                        })
                        .collect::<Option<BTreeMap<String, f64>>>(),
                    ChoiceProbabilities::Map => raw_probabilities
                        .and_then(JsonValue::as_object)
                        .ok_or_else(|| invalid_probabilities(label, id))?
                        .iter()
                        .map(|(key, probability)| {
                            probability
                                .as_f64()
                                .filter(|value| value.is_finite())
                                .map(|probability| (key.clone(), probability))
                        })
                        .collect::<Option<BTreeMap<String, f64>>>(),
                }
                .ok_or_else(|| invalid_probabilities(label, id))?;
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
                if answer_type != Some(bool_type) {
                    return Err(AiError::NonRetryable(format!(
                        "{label} did not return a bool answer for {id}"
                    )));
                }
                let probability = required_number(bool_field)?;
                parsed.insert(id.clone(), ClassifierAnswer::Bool { probability });
            }
        }
    }
    Ok(parsed)
}

fn invalid_probabilities(label: &str, id: &str) -> AiError {
    AiError::NonRetryable(format!("{label} returned invalid probabilities for {id}"))
}
