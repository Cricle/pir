//! Final tool-call argument parsing shared by every wire adapter.
//!
//! Mirrors upstream pi v1.3.0 `packages/ai/src/utils/json-parse.ts`
//! `parseToolCallArguments` (#10897/#10944): a finished call's complete
//! argument text is parsed strictly (only invalid string escapes are
//! repaired). Text that is not a valid JSON *object* never keeps a parsed
//! prefix: the call is kept with `arguments = {}` and `argumentsError`
//! describing the problem plus the text the model sent, so tool validation
//! reports it back to the model and the model can self-correct.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::ai::types::ToolCall;

/// The finalized arguments of one finished tool call plus the upstream
/// `ToolCall.argumentsError` (always `None` on success).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FinalToolArguments {
    pub arguments: BTreeMap<String, Value>,
    pub error: Option<String>,
}

impl FinalToolArguments {
    fn empty() -> Self {
        Self::default()
    }
}

/// Parse the model's complete arguments text of a finished tool call.
///
/// Upstream shape:
/// - empty/whitespace-only text → `{}` with no error;
/// - valid (or escape-repaired) JSON object → the object, no error;
/// - any other JSON value (array, string, number, `null`, …) →
///   `"Tool arguments must be a JSON object\n\nReceived arguments:\n<raw>"`;
/// - malformed JSON → `<serde error>\n\nReceived arguments:\n<raw>`.
pub fn parse_final_tool_arguments(input: &str) -> FinalToolArguments {
    if input.trim().is_empty() {
        return FinalToolArguments::empty();
    }
    let parsed = match serde_json::from_str::<Value>(input) {
        Ok(parsed) => Ok(parsed),
        Err(error) => {
            let repaired = repair_json_string_escapes(input);
            if repaired != input {
                serde_json::from_str::<Value>(&repaired)
            } else {
                Err(error)
            }
        }
    };
    match parsed {
        Ok(Value::Object(map)) => FinalToolArguments {
            arguments: map.into_iter().collect(),
            error: None,
        },
        Ok(_) => FinalToolArguments {
            arguments: BTreeMap::new(),
            error: Some(format!(
                "Tool arguments must be a JSON object\n\nReceived arguments:\n{input}"
            )),
        },
        Err(error) => FinalToolArguments {
            arguments: BTreeMap::new(),
            error: Some(format!("{error}\n\nReceived arguments:\n{input}")),
        },
    }
}

/// Finalize a [`ToolCall`] block in place from its complete argument text:
/// arguments and `argumentsError` are written together, so a failed parse can
/// never leave a stale parsed prefix behind.
pub fn apply_final_tool_arguments(call: &mut ToolCall, input: &str) {
    let parsed = parse_final_tool_arguments(input);
    call.arguments = parsed.arguments;
    call.arguments_error = parsed.error;
}

/// Normalize malformed JSON string literals the way upstream `repairJson`
/// does: escape raw control characters inside strings and double the
/// backslash before an invalid escape character. Structural damage is left
/// untouched, so a truncated document is still rejected by the strict parse.
pub(crate) fn repair_json_string_escapes(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len() + 8);
    let mut in_string = false;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if !in_string {
            output.push(ch);
            if ch == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }
        if ch == '"' {
            output.push(ch);
            in_string = false;
            index += 1;
            continue;
        }
        if ch == '\\' {
            let Some(next) = chars.get(index + 1).copied() else {
                // Trailing backslash: keep a literal backslash.
                output.push_str("\\\\");
                index += 1;
                continue;
            };
            if next == 'u' {
                let digits: String = chars[index + 2..].iter().take(4).collect();
                if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
                    output.push_str("\\u");
                    output.push_str(&digits);
                    index += 6;
                    continue;
                }
            }
            if is_valid_json_escape(next) {
                output.push('\\');
                output.push(next);
                index += 2;
                continue;
            }
            // Invalid escape (`\p`, `\u` without four hex digits, …): double
            // the backslash and leave the following character to the next
            // iteration (upstream does not consume it either).
            output.push_str("\\\\");
            index += 1;
            continue;
        }
        match ch {
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            // Only U+0000..U+001F must be escaped inside a JSON string.
            c if (c as u32) <= 0x1f => output.push_str(&format!("\\u{:04x}", c as u32)),
            c => output.push(c),
        }
        index += 1;
    }
    output
}

/// Characters JSON accepts after a backslash (`\u` is handled separately by
/// its four-hex-digit form).
fn is_valid_json_escape(ch: char) -> bool {
    matches!(ch, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')
}
