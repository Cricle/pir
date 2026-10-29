//! Grammar-leak repair: the port of pi-tool-repair's `src/grammar-repair.ts`
//! (1225 TS lines) onto pir's typed
//! [`AssistantMessage`](crate::ai::types::AssistantMessage) — Step 2 of the
//! scope doc.
//!
//! Upstream tracking point: npm **`pi-tool-repair` 0.3.6**
//! (monotykamary/pi-tool-repair). Behavior truth is the package source read
//! against `~/.pi/agent/npm/node_modules/pi-tool-repair`:
//!
//! - the ten-family dispatch [`parse_tool_grammar_candidates`] mirrors
//!   `parseToolGrammarCandidates` (`grammar-repair.ts:296-322`) — the family
//!   parsers are anchored per function below;
//! - the message pass [`repair_message_grammar_leaks`] mirrors
//!   `repairAssistantMessageGrammarLeaks` (`:193-283`);
//! - the per-model auto-enable gate [`resolve_grammar_repair_for_model`]
//!   mirrors `resolveGrammarRepairForModel` (`:158-170`);
//! - the public extractor [`parse_tool_grammar_leaks`] mirrors
//!   `parseToolGrammarLeaks` (`:285-294`);
//! - the family fixtures in the test suite are drawn from the package's
//!   `docs/tool-call-grammar-leakage-survey.md` §A–J.
//!
//! ## This file parses hostile text — the boundary rules
//!
//! The input is arbitrary model output, so every rule here is total and
//! panic-free (`Result`/`Option`, no indexing that a match did not justify, no
//! `unsafe`). Two invariants keep the byte-index arithmetic sound:
//!
//! - Every offset handed to [`is_inside_code_fence`] or used to slice `text`
//!   comes from a `regex` match (which is UTF-8-boundary aligned) or from
//!   `char_indices`/`str` scanning; `[..]` slicing therefore never splits a
//!   character, including around the fullwidth `｜` (U+FF5C) bars that family A
//!   is built on.
//! - Parsers never invent text: they only produce `(name, arguments, range)`
//!   triples whose range is a span of the input, and the pass removes exactly
//!   those spans. A candidate whose range is empty is dropped
//!   (`:321`), so a parser that "matched" nothing can never delete text.
//!
//! Nothing here executes a call: the recovered calls are handed to
//! [`repair_message_with_grammar`](super::repair::repair_message_with_grammar)
//! and become `ToolCall` blocks only through the gates recorded below.
//!
//! ## Safety gates (the TS source read precisely)
//!
//! - **`requireKnownTool` / empty known set** (`:1065-1068`): when the flag is
//!   on, a candidate is recovered only when its name is in the known/active
//!   set, and an **empty** known set recovers nothing (`knownTools.size > 0 &&
//!   ...`). `stripOnly` candidates bypass the gate — they are markup.
//! - **Code fences** (`:1212-1216`): a ` ``` `-odd prefix means "inside a fenced
//!   block"; every parser that consults it leaves that grammar alone (the
//!   survey's design implication 6).
//! - **Empty arguments** (`:230-238`): a recovered candidate whose grammar
//!   parsed to `{}` is stripped but not recovered (the
//!   `<tool_call>write</tool_call>` case).
//! - **Unterminated blocks** (DSML `:333-335`, Kimi `:484-562`): a block with
//!   no closer is still stripped, but never recovered — except the DSML
//!   trailing-`invoke` recovery arm, which TS does recover from (`:334`), and
//!   the `stripOnly` dangling markers (`:346-366`) which are always
//!   strip-only.
//! - **Existing native tool calls** (`:251`): `shouldRecover` requires
//!   `existingToolCalls.length === 0`, so leaked shadow text is stripped but
//!   never double-recovered.
//! - **Mangled names** (`:178-191`): `bash_1_234` → the longest known prefix
//!   whose remainder matches `^[_-\d]+$`; an unknown name is returned
//!   untouched (and is then rejected by `requireKnownTool`).
//! - **`mode`** (`:251`, `:256-275`): `recover` appends the calls and sets
//!   `stopReason = toolUse`; `strip` removes the markup and stops there.
//! - **`enabled`** (`:198`): the pass is a no-op while the resolved config is
//!   disabled. The caller resolves `enabled || leakModels-match` through
//!   [`resolve_grammar_repair_for_model`]; the GLM token-leak gate is a
//!   *different* gate (`aliases::has_grammar_leak_bug`) and the two never
//!   merge (scope doc §6).
//!
//! ## Conversion-path deviations (recorded, not silent)
//!
//! - **In place instead of returned**: TS returns
//!   `{changed, recoveredCalls, strippedRanges, message}` built from a fresh
//!   content array; this port mutates the message in place and returns the
//!   same three scalars/vectors minus the message. Observable behavior is
//!   identical: parts are only rewritten when the candidate list is non-empty
//!   (which is also what sets `changed`), and the recovered blocks are only
//!   appended when `shouldRecover` holds.
//! - **`message.role`/`Array.isArray(message.content)` guards** (`:198`)
//!   collapse: an `AssistantMessage` is always the assistant role with an
//!   array of blocks.
//! - **Recovered id**: `tool_repair_<grammar>_<base36>_<i>` like
//!   `makeRecoveredToolCallId` (`:1098-1100`), with the per-process
//!   `AtomicU64` counter in place of `Date.now()` — no clock read keeps the
//!   pass pure, and the counter is strictly more collision-proof.
//! - **Key order**: `ToolCall.arguments` is a `BTreeMap`, so the appended
//!   call's arguments lose the model's key order (sorted instead), the same
//!   transcript-byte note repair.rs records for repaired calls.
//! - **Regex dialect**: JS patterns are translated one-for-one into the Rust
//!   `regex` crate. The only deltas are character-class corners — Rust's `\s`
//!   adds U+0085 and drops U+FEFF, `\w`/`\d` are Unicode-aware where JS's are
//!   ASCII (the identifier patterns are therefore spelled
//!   `[A-Za-z_][A-Za-z0-9_.-]*`), and `str::trim` follows
//!   `char::is_whitespace` where JS trims a slightly different set. None of
//!   them are reachable through the *validation* gates above: a name that
//!   only differs in such a character cannot be in the active tool set, so it
//!   is never recovered.
//! - **JS `.replace(/\bTrue\b/g, …)`** in `parsePythonishValue` is ported with
//!   `\b`-anchored regexes, not plain substring replacement.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use regex::Regex;
use serde_json::{Map, Value as JsonValue};

use crate::ai::types::{AssistantMessage, ContentBlock, StopReason, ToolCall};

use super::config::{GrammarRepairConfig, GrammarRepairMode};

// ---------------------------------------------------------------------------
// Public shapes (`grammar-repair.ts:36-40`, `:82-87`)
// ---------------------------------------------------------------------------

/// One recovered call (`RecoveredToolCall`, `:36-40`).
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredToolCall {
    /// The tool name, after mangled-name recovery.
    pub name: String,
    /// The parsed arguments. An object here is always `{}` or richer.
    pub arguments: Map<String, JsonValue>,
    /// The grammar family that produced it, one of
    /// [`GRAMMAR_NAMES`](super::aliases::GRAMMAR_NAMES).
    pub grammar: &'static str,
}

/// What one message pass did (`GrammarRepairResult`, `:82-87`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GrammarRepairResult {
    /// Whether the pass stripped and/or recovered anything.
    pub changed: bool,
    /// The recovered calls; empty unless `mode == recover` *and* the message
    /// carried no native tool call *and* at least one call survived the gates.
    pub recovered_calls: Vec<RecoveredToolCall>,
    /// Number of strip ranges removed across all text/thinking parts
    /// (`strippedRanges`).
    pub stripped_ranges: usize,
}

/// One byte span of an input string (`Range`, `:47-50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GrammarRange {
    start: usize,
    end: usize,
}

impl GrammarRange {
    fn len(&self) -> usize {
        self.end - self.start
    }
}

/// One parsed candidate (`Candidate`, `:42-45`).
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    name: String,
    arguments: Map<String, JsonValue>,
    grammar: &'static str,
    range: GrammarRange,
    /// A marker to strip only: never recovered, always stripped (`:44`).
    strip_only: bool,
}

/// A parser result before the grammar/range are attached.
#[derive(Debug, Clone, PartialEq)]
struct ParsedCall {
    name: String,
    arguments: Map<String, JsonValue>,
}

/// The JSON value of an argument name lookup that skips `null`
/// (JS `??` treats `null` and `undefined` alike).
fn non_null<'a>(map: &'a Map<String, JsonValue>, key: &str) -> Option<&'a JsonValue> {
    map.get(key).filter(|value| !value.is_null())
}

// ---------------------------------------------------------------------------
// Regex inventory (each mirrors one TS literal; anchors name the TS function)
// ---------------------------------------------------------------------------

/// JS `[A-Za-z_][\w.-]*` spelled in ASCII so Rust's Unicode `\w` cannot widen
/// the accepted identifier set.
const IDENT: &str = r"[A-Za-z_][A-Za-z0-9_.-]*";

/// The DSML bar prefix (`parseDsml`, `:326`): double bar, single bar, no
/// leading bar, and the transcoded ASCII-pipe variant.
const DSML_PREFIX: &str = r"(?:｜{1,2}DSML｜{1,2}|DSML｜|\s*\|\s*DSML\s*\|\s*)";

/// `Regex::new` on a static literal cannot fail; a failure is a typo caught by
/// the suite on first use rather than a silent pattern drop.
fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static grammar-repair pattern")
}

// -- A. DeepSeek DSML (`parseDsml` :324-344, `parseDsmlDanglingMarkers` :346-366)

static DSML_OUTER_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)<{DSML_PREFIX}(?:tool_calls|function_calls)>"
    ))
});
static DSML_TOOL_CALLS_CLOSE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"(?i)</{DSML_PREFIX}tool_calls>")));
static DSML_FUNCTION_CALLS_CLOSE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"(?i)</{DSML_PREFIX}function_calls>")));
static DSML_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)</?{DSML_PREFIX}(?:tool_calls|function_calls|invoke|parameter)(?:\s+[^>\n]*)?>?"
    ))
});
static DSML_INVOKE: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)<{DSML_PREFIX}invoke\s+name=["']([^"']+)["']\s*>"#
    ))
});
static DSML_INVOKE_CLOSE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"(?i)</{DSML_PREFIX}invoke>")));
static DSML_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)<{DSML_PREFIX}parameter\s+name=["']([^"']+)["'](?:\s+string=["'](true|false)["'])?\s*>([\s\S]*?)</{DSML_PREFIX}parameter>"#
    ))
});

// -- D. Kimi sentinels (`parseKimi` :429-455, `parseKimiDanglingMarkers` :484-537)

static KIMI_SECTION: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)<\|tool_calls?_section_begin\|>([\s\S]*?)<\|tool_calls?_section_end\|>")
});
static KIMI_CALL: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)<\|tool_call_begin\|>([^<]*?)<\|tool_call_argument_begin\|>([\s\S]*?)<\|tool_call_end\|>",
    )
});
static KIMI_SECTION_OPEN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<\|(?:redacted_)?tool_calls?_section_begin\|>"));
static KIMI_SECTION_END: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<\|(?:redacted_)?tool_calls?_section_end\|>"));
static KIMI_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)<\|(?:redacted_)?tool_call(?:s)?(?:_section)?_(?:begin(?:_kimi)?|argument_begin|end(?:_kimi)?)\|>",
    )
});
static KIMI_ARG_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<\|(?:redacted_)?tool_call_argument_begin\|>"));
static KIMI_ID_TAIL: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^\s*functions\.{IDENT}:[0-9]*")));
static KIMI_ID_CANONICAL: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^functions\.({IDENT}):[0-9]+$")));
static KIMI_ID_RELAXED: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^(?:functions\.)?({IDENT})(?::[0-9]+)?$")));
static KIMI_CALL_ID_GUARD: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)^call[_-]?[0-9]+$"));
/// `<tool_call>` / `</tool_call>` (also used by the orphan-closer counter).
static KIMI_OPEN_TOOL_CALL: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)<tool_call>"));
static CLOSE_TOOL_CALL: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)</tool_call>"));
/// The loose Kimi closer: a reasoning-block close acts as the call's end
/// (`parseKimiLooseToolCallXml`, `:468-472`).
static CLOSE_REASONING: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)</(?:redacted_thinking|thinking)>"));
/// `parseOrphanToolCallClosers` (`:566`): the closer plus its trailing
/// whitespace is the strip range.
static ORPHAN_TOOL_CALL_CLOSE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)</tool_call>\s*"));

// -- C/G/H. `<tool_call>` wrappers and their bodies (`parseToolCallXml` :742-785)

static TOOL_CALL_XML_OPEN: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)<(tool_call|tools)>"));
static CLOSE_TOOL_CALL_TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)</tool_call>"));
static CLOSE_TOOLS_TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)</tools>"));
static BARE_QWEN_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"(?i)<function=({IDENT})>[\s\S]*?</function>")));
static QWEN_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)<function=({IDENT})>\s*([\s\S]*?)</function>"
    ))
});
static QWEN_PARAM: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<parameter=([^>]+)>([\s\S]*?)</parameter>"));
static GLM_ARG_KEY: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)<arg_key>([\s\S]*?)</arg_key>"));
static GLM_ARG_VALUE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<arg_value>([\s\S]*?)</arg_value>"));
static IDENT_FULL: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"^({IDENT})$")));

// -- B. `<invoke>` / `<parameter>` (`parseInvokeXml` :668-691, malformed :721-740)

static INVOKE_WRAPPED: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)<(?:{IDENT}:)?tool_call>([\s\S]*?)</(?:{IDENT}:)?tool_call>"
    ))
});
static INVOKE_STANDALONE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)<invoke\s+name=["']([^"']+)["']\s*>[\s\S]*?</invoke>"#));
static INVOKE_BODY: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)<invoke\s+name=["']([^"']+)["']\s*>([\s\S]*?)</invoke>"#));
static INVOKE_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)<parameter\s+name=["']([^"']+)["'](?:\s+string=["'](true|false)["'])?\s*>([\s\S]*?)</parameter>"#,
    )
});
static MALFORMED_INVOKE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)(?:^|\n)(\s*)invoke\s+name=["']([^"']+)["']\s*>([\s\S]*?)(?:\n\s*(?:/invoke|invoke)>|$)"#,
    )
});
static MALFORMED_PARAM: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)parameter\s+name=["']([^"']+)["']\s*>([\s\S]*?)\s*parameter>"#));

// -- E/F. Mistral sentinels and Llama python_tag (`parseMistral` :595-646,
//    `parseLlamaPythonTag` :847-880, `parseBareJsonToolCalls` :882-906)

static MISTRAL_V11: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^({IDENT})\[CALL_ID\]([^\[]*)\[ARGS\]")));
static BARE_JSON_OBJECT: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"\{(?:\s)*"(?:name|function_name|function)""#));
static BARE_JSON_ARRAY: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"\[\s*\{\s*"(?:name|function_name|function)""#));
static PYTHON_TAG: &str = "<|python_tag|>";

// -- I/J. MiniMax-Text-01 and OLMo (`parseMiniMaxText01` :648-666, `parseOlmo` :927-937)

static MINIMAX_TEXT_CALL: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)<function_call>[\s\S]*?functions\.({IDENT})\s*\("
    ))
});
static OLMO_FUNCTION_CALLS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)<function_calls>([\s\S]*?)</function_calls>"));

// -- Pythonic calls (`parsePythonicCalls` :939-952, `parsePythonishValue` :966-976)

static PYTHONIC_CALL_LINE: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"(?:^|\n)\s*({IDENT})\s*\(")));
static PY_NUMBER: LazyLock<Regex> = LazyLock::new(|| regex(r"^[-+]?[0-9]+(?:\.[0-9]+)?$"));
static PY_TRUE_WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"\bTrue\b"));
static PY_FALSE_WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"\bFalse\b"));
static PY_NONE_WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"\bNone\b"));

// -- Shared primitives

static MARKDOWN_FENCE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?s)^```[A-Za-z0-9_]*\s*(.*?)\s*```$"));
static MAYBE_JSON_VALUE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"^(?:true|false|null|-?[0-9]|[\[{]|")"#));
static MANGLED_TOOL_SUFFIX: LazyLock<Regex> = LazyLock::new(|| regex(r"^[_0-9-]+$"));
/// `removeRanges` cleanup (`:1079`).
static SPACE_BEFORE_NEWLINE: LazyLock<Regex> = LazyLock::new(|| regex(r"[ \t]+\n"));
static EXCESS_NEWLINES: LazyLock<Regex> = LazyLock::new(|| regex(r"\n{3,}"));

// ---------------------------------------------------------------------------
// Shared primitives (`:1102-1225`)
// ---------------------------------------------------------------------------

/// `findPattern` (`:1102-1107`): leftmost match at or after `from`.
fn find_at(pattern: &Regex, text: &str, from: usize) -> Option<GrammarRange> {
    pattern.find_at(text, from).map(|matched| GrammarRange {
        start: matched.start(),
        end: matched.end(),
    })
}

/// `findLineEnd` (`:1207-1210`).
fn find_line_end(text: &str, start: usize) -> usize {
    text[start..]
        .find('\n')
        .map(|offset| start + offset)
        .unwrap_or(text.len())
}

/// `isInsideCodeFence` (`:1212-1216`): an odd number of ` ``` ` before the
/// index means the index sits inside a fenced block. The count is deliberately
/// naive — the TS one is, and the guard is a heuristic, not a Markdown parser.
fn is_inside_code_fence(text: &str, index: usize) -> bool {
    text[..index].matches("```").count() % 2 == 1
}

/// `unwrapMarkdownFence` (`:1218-1221`).
fn unwrap_markdown_fence(text: &str) -> String {
    match MARKDOWN_FENCE.captures(text) {
        Some(captures) => captures
            .get(1)
            .map(|body| body.as_str().to_owned())
            .unwrap_or_default(),
        None => text.to_owned(),
    }
}

/// `extractFirstBalancedJson` (`:1109-1117`): the first `{...}`/`[...]` span,
/// quote- and escape-aware. `end` is exclusive.
struct BalancedJson {
    json: String,
    end: usize,
}

fn extract_first_balanced_json(text: &str) -> Option<BalancedJson> {
    let (start, opener) = text
        .char_indices()
        .find(|(_, ch)| *ch == '{' || *ch == '[')?;
    let closer = if opener == '{' { '}' } else { ']' };
    let end = find_matching(text, start, opener, closer)?;
    Some(BalancedJson {
        json: text[start..=end].to_owned(),
        end: end + 1,
    })
}

/// `findMatching` (`:1119-1149`): the index of the closer that balances the
/// opener at `open_index`, skipping quoted runs (both quote styles, with
/// backslash escapes).
fn find_matching(text: &str, open_index: usize, opener: char, closer: char) -> Option<usize> {
    let mut depth: i64 = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (offset, ch) in text[open_index..].char_indices() {
        let index = open_index + offset;
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == opener {
            depth += 1;
        }
        if ch == closer {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }

    None
}

/// `splitTopLevel` (`:1151-1180`): split on `delimiter` outside quotes and
/// brackets, then trim and drop empty parts.
fn split_top_level(text: &str, delimiter: char) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut depth: i64 = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (index, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == '(' || ch == '[' || ch == '{' {
            depth += 1;
        }
        if ch == ')' || ch == ']' || ch == '}' {
            depth -= 1;
        }
        if ch == delimiter && depth == 0 {
            parts.push(text[start..index].to_owned());
            start = index + ch.len_utf8();
        }
    }
    parts.push(text[start..].to_owned());

    parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

/// `findTopLevelChar` (`:1182-1205`).
fn find_top_level_char(text: &str, target: char) -> Option<usize> {
    let mut depth: i64 = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (index, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == '(' || ch == '[' || ch == '{' {
            depth += 1;
        }
        if ch == ')' || ch == ']' || ch == '}' {
            depth -= 1;
        }
        if ch == target && depth == 0 {
            return Some(index);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// JSON helpers (`:1012-1041`)
// ---------------------------------------------------------------------------

/// `parseJsonValue` (`:1022-1028`): `JSON.parse` or nothing.
fn parse_json_value(json: &str) -> Option<JsonValue> {
    serde_json::from_str(json).ok()
}

/// `parseJsonObject` (`:1017-1020`).
fn parse_json_object(json: &str) -> Option<Map<String, JsonValue>> {
    match parse_json_value(json) {
        Some(JsonValue::Object(map)) => Some(map),
        _ => None,
    }
}

/// `parseJsonValueOrString` (`:1030-1033`).
fn parse_json_value_or_string(value: &str) -> JsonValue {
    parse_json_value(value).unwrap_or_else(|| JsonValue::String(value.to_owned()))
}

/// `maybeParseJsonValue` (`:1035-1041`).
fn maybe_parse_json_value(value: &str) -> JsonValue {
    if value.is_empty() {
        return JsonValue::String(String::new());
    }
    if MAYBE_JSON_VALUE.is_match(value) {
        parse_json_value_or_string(value)
    } else {
        JsonValue::String(value.to_owned())
    }
}

/// `normalizeArgumentsObject` (`:997-1010`): a string becomes its JSON parse,
/// and an object with a nested `arguments` string/object unwraps it (recursively)
/// in preference to itself.
fn normalize_arguments_object(value: &JsonValue) -> Option<Map<String, JsonValue>> {
    match value {
        JsonValue::String(text) => {
            parse_json_value(text).and_then(|parsed| normalize_arguments_object(&parsed))
        }
        JsonValue::Object(map) => {
            if let Some(nested) = map.get("arguments") {
                if nested.is_string() || nested.is_object() {
                    if let Some(unwrapped) = normalize_arguments_object(nested) {
                        return Some(unwrapped);
                    }
                }
            }
            Some(map.clone())
        }
        _ => None,
    }
}

/// `callFromJsonObject` (`:987-995`): the OpenAI-ish `{name, arguments}`
/// shape, with `function_name`/`args`/`parameters`/`{function: {...}}`
/// fallbacks. Empty or non-string names are not calls.
fn call_from_json_object(value: &Map<String, JsonValue>) -> Option<ParsedCall> {
    let name_value = non_null(value, "name")
        .or_else(|| non_null(value, "function_name"))
        .or_else(|| {
            value
                .get("function")
                .filter(|function| function.is_object())
                .and_then(|function| function.get("name"))
        });
    let name = name_value.and_then(JsonValue::as_str)?;
    if name.trim().is_empty() {
        return None;
    }

    let mut args = non_null(value, "arguments")
        .or_else(|| non_null(value, "args"))
        .or_else(|| non_null(value, "parameters"));
    if args.is_none() {
        args = value
            .get("function")
            .filter(|function| function.is_object())
            .and_then(|function| function.get("arguments"));
    }
    let arguments = args
        .and_then(normalize_arguments_object)
        .unwrap_or_default();
    Some(ParsedCall {
        name: name.trim().to_owned(),
        arguments,
    })
}

/// `callsFromJsonValue` (`:978-985`): arrays flatten recursively; other values
/// yield the call a matching object describes, or nothing.
fn calls_from_json_value(value: &JsonValue) -> Vec<ParsedCall> {
    match value {
        JsonValue::Array(items) => items.iter().flat_map(calls_from_json_value).collect(),
        JsonValue::Object(map) => call_from_json_object(map).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// `parseToolCallJsonBody` (`:787-809`): the body is a JSON object, or an array
/// of call objects, possibly wrapped in a Markdown fence; the caller attaches
/// the wrapper's range afterwards (the placeholder range below is always
/// rewritten, and [`parse_tool_grammar_candidates`] drops empty ranges).
fn parse_tool_call_json_body(body: &str, grammar: &'static str) -> Vec<Candidate> {
    let mut calls = Vec::new();
    let trimmed = unwrap_markdown_fence(body.trim());
    let json = extract_first_balanced_json(&trimmed)
        .map(|extracted| extracted.json)
        .unwrap_or(trimmed);
    let Some(parsed) = parse_json_value(&json) else {
        return calls;
    };

    match &parsed {
        JsonValue::Array(items) => {
            for item in items {
                if let JsonValue::Object(map) = item {
                    if let Some(call) = call_from_json_object(map) {
                        calls.push(json_call_candidate(call, grammar));
                    }
                }
            }
        }
        JsonValue::Object(map) => {
            if let Some(call) = call_from_json_object(map) {
                calls.push(json_call_candidate(call, grammar));
            }
        }
        _ => {}
    }

    calls
}

/// One `callFromJsonObject` result as a range-less candidate; the caller
/// attaches the wrapper span.
fn json_call_candidate(call: ParsedCall, grammar: &'static str) -> Candidate {
    Candidate {
        name: call.name,
        arguments: call.arguments,
        grammar,
        range: GrammarRange { start: 0, end: 0 },
        strip_only: false,
    }
}

// ---------------------------------------------------------------------------
// A. DeepSeek DSML (`:324-427`)
// ---------------------------------------------------------------------------

/// `parseDsml` (`:324-344`): each outer `tool_calls`/`function_calls` block is
/// one strip range; every closed `<invoke>` inside contributes a candidate.
/// The TS close search tries `tool_calls` first, then `function_calls`,
/// whatever the opener was (`:333`).
fn parse_dsml(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for outer in DSML_OUTER_OPEN.find_iter(text) {
        let (start, body_start) = (outer.start(), outer.end());
        if is_inside_code_fence(text, start) {
            continue;
        }
        let close = find_at(&DSML_TOOL_CALLS_CLOSE, text, body_start)
            .or_else(|| find_at(&DSML_FUNCTION_CALLS_CLOSE, text, body_start));
        let end = match &close {
            Some(close) => close.end,
            None => match find_best_unclosed_dsml_end(text, body_start) {
                Some(end) => end,
                None => continue,
            },
        };
        let body_end = close.map(|close| close.start).unwrap_or(end);
        let body = &text[body_start..body_end];

        for call in parse_dsml_invokes(body) {
            candidates.push(Candidate {
                name: call.name,
                arguments: call.arguments,
                grammar: "dsml",
                range: GrammarRange { start, end },
                strip_only: false,
            });
        }
    }

    candidates
}

/// `parseDsmlDanglingMarkers` (`:346-366`): `DSML`-looking markers that never
/// formed a block (e.g. a stream that died at `<｜DSML｜tool_calls`) are
/// strip-only so raw grammar does not persist in the transcript.
fn parse_dsml_dangling_markers(text: &str) -> Vec<Candidate> {
    if !text.contains("DSML") {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for marker in DSML_MARKER.find_iter(text) {
        if is_inside_code_fence(text, marker.start()) {
            continue;
        }
        candidates.push(Candidate {
            name: String::new(),
            arguments: Map::new(),
            grammar: "dsml",
            range: GrammarRange {
                start: marker.start(),
                end: marker.end(),
            },
            strip_only: true,
        });
    }
    candidates
}

/// `findBestUnclosedDsmlEnd` (`:376-386`): the *last* `</…invoke>` at or after
/// `from` is treated as the end of an unterminated outer block.
fn find_best_unclosed_dsml_end(text: &str, from: usize) -> Option<usize> {
    let mut end = None;
    for matched in DSML_INVOKE_CLOSE.find_iter(&text[from..]) {
        end = Some(from + matched.end());
    }
    end
}

/// `parseDsmlInvokes` (`:388-405`).
fn parse_dsml_invokes(body: &str) -> Vec<ParsedCall> {
    let mut calls = Vec::new();

    for invoke in DSML_INVOKE.captures_iter(body) {
        let Some(name) = invoke.get(1).map(|name| name.as_str().trim()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Some(open) = invoke.get(0) else {
            continue;
        };
        let body_start = open.end();
        let Some(close) = find_at(&DSML_INVOKE_CLOSE, body, body_start) else {
            continue;
        };
        let invoke_body = &body[body_start..close.start];
        calls.push(ParsedCall {
            name: name.to_owned(),
            arguments: parse_dsml_arguments(invoke_body),
        });
    }

    calls
}

/// `parseDsmlArguments` (`:407-427`): `<parameter string="true">` keeps the
/// raw (possibly multi-line) value; `string="false"` JSON-parses it. A body
/// with no parameters falls back to a direct JSON object.
fn parse_dsml_arguments(body: &str) -> Map<String, JsonValue> {
    let mut args = Map::new();

    for param in DSML_PARAM.captures_iter(body) {
        let Some(key) = param.get(1).map(|key| key.as_str().trim()) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        let string_attr = param.get(2).map(|value| value.as_str());
        let raw_value = param.get(3).map(|value| value.as_str()).unwrap_or("");
        let value = if string_attr == Some("false") {
            parse_json_value_or_string(raw_value.trim())
        } else {
            JsonValue::String(raw_value.to_owned())
        };
        args.insert(key.to_owned(), value);
    }

    if !args.is_empty() {
        return args;
    }

    let trimmed = body.trim();
    let json = extract_first_balanced_json(trimmed)
        .map(|extracted| extracted.json)
        .unwrap_or_else(|| trimmed.to_owned());
    parse_json_object(&json)
        .and_then(|direct| normalize_arguments_object(&JsonValue::Object(direct)))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// D. Kimi sentinels (`:429-593`)
// ---------------------------------------------------------------------------

/// `parseKimi` (`:429-455`): the closed sentinel form, plus the loose
/// `<tool_call>` form and the dangling-marker sweep.
fn parse_kimi(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for section in KIMI_SECTION.captures_iter(text) {
        let (Some(whole), Some(body)) = (section.get(0), section.get(1)) else {
            continue;
        };
        if is_inside_code_fence(text, whole.start()) {
            continue;
        }
        let range = GrammarRange {
            start: whole.start(),
            end: whole.end(),
        };
        for call in KIMI_CALL.captures_iter(body.as_str()) {
            let id_text = call.get(1).map(|id| id.as_str()).unwrap_or("").trim();
            let Some(name) = parse_kimi_tool_name(id_text) else {
                continue;
            };
            let raw_args = call.get(2).map(|args| args.as_str()).unwrap_or("").trim();
            candidates.push(Candidate {
                name,
                arguments: parse_json_object(raw_args).unwrap_or_default(),
                grammar: "kimi",
                range,
                strip_only: false,
            });
        }
    }

    candidates.extend(parse_kimi_loose_tool_call_xml(text));
    candidates.extend(parse_kimi_dangling_markers(text));
    candidates
}

/// `parseKimiLooseToolCallXml` (`:457-482`): an unclosed `<tool_call>` whose
/// body ends at the reasoning block's closer.
fn parse_kimi_loose_tool_call_xml(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for open in KIMI_OPEN_TOOL_CALL.find_iter(text) {
        let (start, body_start) = (open.start(), open.end());
        if is_inside_code_fence(text, start) {
            continue;
        }
        if find_at(&CLOSE_TOOL_CALL, text, body_start).is_some() {
            continue;
        }
        let Some(loose_close) = find_at(&CLOSE_REASONING, text, body_start) else {
            continue;
        };
        let body = &text[body_start..loose_close.start];
        for call in parse_tool_call_json_body(body, "kimi") {
            candidates.push(Candidate {
                range: GrammarRange {
                    start,
                    end: loose_close.end,
                },
                ..call
            });
        }
    }

    candidates
}

/// `parseKimiDanglingMarkers` (`:484-537`): sentinel openers with no closer,
/// unclosed `<tool_call>` blocks (closer optional), and every remaining
/// sentinel token — all strip-only, deduplicated by overlap against the
/// candidates this function already produced.
fn parse_kimi_dangling_markers(text: &str) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();

    for open in KIMI_SECTION_OPEN.find_iter(text) {
        let section_start = open.start();
        if is_inside_code_fence(text, section_start) {
            continue;
        }
        if find_at(&KIMI_SECTION_END, text, section_start).is_some() {
            continue;
        }
        let end = find_best_kimi_unclosed_end(text, open.end()).unwrap_or(open.end());
        candidates.push(Candidate {
            name: String::new(),
            arguments: Map::new(),
            grammar: "kimi",
            range: GrammarRange {
                start: section_start,
                end,
            },
            strip_only: true,
        });
    }

    for open in KIMI_OPEN_TOOL_CALL.find_iter(text) {
        let (start, body_start) = (open.start(), open.end());
        if is_inside_code_fence(text, start) {
            continue;
        }
        let strict_close = find_at(&CLOSE_TOOL_CALL, text, body_start);
        let loose_close = find_at(&CLOSE_REASONING, text, body_start);
        let close = strict_close.or(loose_close);
        if let Some(close) = close {
            if !parse_tool_call_json_body(&text[body_start..close.start], "kimi").is_empty() {
                continue;
            }
        }
        candidates.push(Candidate {
            name: String::new(),
            arguments: Map::new(),
            grammar: "kimi",
            range: GrammarRange {
                start,
                end: close.map(|close| close.end).unwrap_or(body_start),
            },
            strip_only: true,
        });
    }

    for marker in KIMI_MARKER.find_iter(text) {
        if is_inside_code_fence(text, marker.start()) {
            continue;
        }
        let range = GrammarRange {
            start: marker.start(),
            end: marker.end(),
        };
        if candidates
            .iter()
            .any(|candidate| ranges_overlap(&candidate.range, &range))
        {
            continue;
        }
        candidates.push(Candidate {
            name: String::new(),
            arguments: Map::new(),
            grammar: "kimi",
            range,
            strip_only: true,
        });
    }

    candidates
}

/// `findBestKimiUnclosedEnd` (`:539-562`): the end of the last sentinel token,
/// extended over a following `functions.name:idx` id and over an
/// `argument_begin` plus one balanced JSON value.
fn find_best_kimi_unclosed_end(text: &str, from: usize) -> Option<usize> {
    let mut end = None;
    for matched in KIMI_MARKER.find_iter(&text[from..]) {
        end = Some(from + matched.end());
    }
    let mut end = end?;

    if let Some(id) = KIMI_ID_TAIL.find(&text[end..]) {
        end += id.end();
    }
    if let Some(marker) = find_at(&KIMI_ARG_BEGIN, text, end) {
        end = marker.end;
        if let Some(json) = extract_first_balanced_json(&text[end..]) {
            end += json.end;
        }
    }

    Some(end)
}

/// `parseOrphanToolCallClosers` (`:564-585`): a `</tool_call>` with no
/// unclosed `<tool_call>` before it is leftover markup for the qwen/granite/
/// kimi families. (`</tool_call>` never matches the `<tool_call>` counter.)
fn parse_orphan_tool_call_closers(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for close in ORPHAN_TOOL_CALL_CLOSE.find_iter(text) {
        if is_inside_code_fence(text, close.start()) {
            continue;
        }
        let before = &text[..close.start()];
        let opens = KIMI_OPEN_TOOL_CALL.find_iter(before).count();
        let closes = CLOSE_TOOL_CALL.find_iter(before).count();
        if opens > closes {
            continue;
        }
        candidates.push(Candidate {
            name: String::new(),
            arguments: Map::new(),
            grammar: "kimi",
            range: GrammarRange {
                start: close.start(),
                end: close.end(),
            },
            strip_only: true,
        });
    }

    candidates
}

/// `parseKimiToolName` (`:587-593`): `functions.{name}:{idx}` canonical, a
/// relaxed `{name}[:idx]` fallback, and an OpenAI-style `call00003` guard.
fn parse_kimi_tool_name(id_text: &str) -> Option<String> {
    if let Some(canonical) = KIMI_ID_CANONICAL.captures(id_text) {
        return canonical.get(1).map(|name| name.as_str().to_owned());
    }
    if let Some(relaxed) = KIMI_ID_RELAXED.captures(id_text) {
        let name = relaxed.get(1)?.as_str();
        if !KIMI_CALL_ID_GUARD.is_match(name) {
            return Some(name.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// E/F. Mistral sentinels and Llama python_tag (`:595-646`, `:847-906`)
// ---------------------------------------------------------------------------

/// `parseMistral` (`:595-646`): the v11 `name[CALL_ID]id[ARGS]{json}` form and
/// the bracketed JSON-array form, scanning every `[TOOL_CALLS]` marker outside
/// code fences.
fn parse_mistral(text: &str) -> Vec<Candidate> {
    const MARKER: &str = "[TOOL_CALLS]";
    let mut candidates = Vec::new();
    let mut index = 0usize;

    while let Some(found) = text[index..].find(MARKER) {
        index += found;
        if is_inside_code_fence(text, index) {
            index += MARKER.len();
            continue;
        }

        let after_marker = index + MARKER.len();
        let rest = text[after_marker..].trim_start();
        let whitespace = text[after_marker..].len() - rest.len();
        let json_start = after_marker + whitespace;

        if rest.starts_with('[') {
            if let Some(extracted) = extract_first_balanced_json(&text[json_start..]) {
                if extracted.json.starts_with('[') {
                    if let Some(JsonValue::Array(items)) = parse_json_value(&extracted.json) {
                        for item in &items {
                            if let JsonValue::Object(map) = item {
                                if let Some(call) = call_from_json_object(map) {
                                    candidates.push(Candidate {
                                        name: call.name,
                                        arguments: call.arguments,
                                        grammar: "mistral",
                                        range: GrammarRange {
                                            start: index,
                                            end: json_start + extracted.end,
                                        },
                                        strip_only: false,
                                    });
                                }
                            }
                        }
                    }
                    index = json_start + extracted.end;
                    continue;
                }
            }
        }

        if let Some(v11) = MISTRAL_V11.captures(rest) {
            let name = v11.get(1).map(|name| name.as_str().to_owned());
            let matched_len = v11.get(0).map(|whole| whole.as_str().len()).unwrap_or(0);
            let args_start = json_start + matched_len;
            if let (Some(name), Some(extracted)) =
                (name, extract_first_balanced_json(&text[args_start..]))
            {
                let arguments = parse_json_object(&extracted.json)
                    .and_then(|parsed| normalize_arguments_object(&JsonValue::Object(parsed)))
                    .unwrap_or_default();
                candidates.push(Candidate {
                    name,
                    arguments,
                    grammar: "mistral",
                    range: GrammarRange {
                        start: index,
                        end: args_start + extracted.end,
                    },
                    strip_only: false,
                });
                index = args_start + extracted.end;
                continue;
            }
        }

        index += MARKER.len();
    }

    candidates
}

/// `parseLlamaPythonTag` (`:847-880`): after `<|python_tag|>`, one balanced
/// JSON value (object or array of call objects) or, failing that, pythonic
/// calls on the same line.
fn parse_llama_python_tag(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut index = 0usize;

    while let Some(found) = text[index..].find(PYTHON_TAG) {
        index += found;
        if is_inside_code_fence(text, index) {
            index += PYTHON_TAG.len();
            continue;
        }
        let body_start = index + PYTHON_TAG.len();
        let rest = text[body_start..].trim_start();
        let whitespace = text[body_start..].len() - rest.len();
        let payload_start = body_start + whitespace;

        if let Some(extracted) = extract_first_balanced_json(&text[payload_start..]) {
            if let Some(parsed) = parse_json_value(&extracted.json) {
                for call in calls_from_json_value(&parsed) {
                    candidates.push(Candidate {
                        name: call.name,
                        arguments: call.arguments,
                        grammar: "llama",
                        range: GrammarRange {
                            start: index,
                            end: payload_start + extracted.end,
                        },
                        strip_only: false,
                    });
                }
            }
            index = payload_start + extracted.end;
            continue;
        }

        let line_end = find_line_end(text, payload_start);
        for call in parse_pythonic_calls(&text[payload_start..line_end]) {
            candidates.push(Candidate {
                name: call.name,
                arguments: call.arguments,
                grammar: "llama",
                range: GrammarRange {
                    start: index,
                    end: line_end,
                },
                strip_only: false,
            });
        }
        index = line_end;
    }

    candidates
}

/// `parseBareJsonToolCalls` (`:882-906`): a `{"name": …}` object or
/// `[{"name": …}]` array appearing as plain text (the `[TOOL_CALLS]` token was
/// dropped). Case-sensitive, like the TS pattern.
fn parse_bare_json_tool_calls(text: &str, grammar: &'static str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for pattern in [&*BARE_JSON_OBJECT, &*BARE_JSON_ARRAY] {
        for matched in pattern.find_iter(text) {
            if is_inside_code_fence(text, matched.start()) {
                continue;
            }
            let Some(extracted) = extract_first_balanced_json(&text[matched.start()..]) else {
                continue;
            };
            let Some(parsed) = parse_json_value(&extracted.json) else {
                continue;
            };
            let end = matched.start() + extracted.end;
            for call in calls_from_json_value(&parsed) {
                candidates.push(Candidate {
                    name: call.name,
                    arguments: call.arguments,
                    grammar,
                    range: GrammarRange {
                        start: matched.start(),
                        end,
                    },
                    strip_only: false,
                });
            }
        }
    }

    candidates
}

// ---------------------------------------------------------------------------
// I. MiniMax-Text-01 (`:648-666`)
// ---------------------------------------------------------------------------

/// `parseMiniMaxText01` (`:648-666`): `<function_call>```typescript
/// functions.name({json})``` `, where the fence end extends the strip range.
fn parse_minimax_text01(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for matched in MINIMAX_TEXT_CALL.captures_iter(text) {
        let (Some(whole), Some(name)) = (matched.get(0), matched.get(1)) else {
            continue;
        };
        if is_inside_code_fence(text, whole.start()) {
            continue;
        }
        let open_paren = whole.end() - 1;
        let Some(close_paren) = find_matching(text, open_paren, '(', ')') else {
            continue;
        };
        let raw_args = text[open_paren + 1..close_paren].trim();
        let arguments = parse_json_object(raw_args)
            .and_then(|parsed| normalize_arguments_object(&JsonValue::Object(parsed)))
            .unwrap_or_default();
        let end = match text[close_paren..].find("```") {
            Some(offset) => close_paren + offset + 3,
            None => close_paren + 1,
        };
        candidates.push(Candidate {
            name: name.as_str().to_owned(),
            arguments,
            grammar: "minimax-text",
            range: GrammarRange {
                start: whole.start(),
                end,
            },
            strip_only: false,
        });
    }

    candidates
}

// ---------------------------------------------------------------------------
// B. `<invoke>` / `<parameter>` (`:668-740`)
// ---------------------------------------------------------------------------

/// `parseInvokeXml` (`:668-691`): a namespaced `<mini:tool_call>` wrapper, a
/// standalone `<invoke name=…>` block, and the bracket-dropped MiniMax variant.
fn parse_invoke_xml(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for wrapper in INVOKE_WRAPPED.captures_iter(text) {
        let (Some(whole), Some(body)) = (wrapper.get(0), wrapper.get(1)) else {
            continue;
        };
        if is_inside_code_fence(text, whole.start()) {
            continue;
        }
        for call in parse_invoke_body(body.as_str()) {
            candidates.push(Candidate {
                name: call.name,
                arguments: call.arguments,
                grammar: "invoke",
                range: GrammarRange {
                    start: whole.start(),
                    end: whole.end(),
                },
                strip_only: false,
            });
        }
    }

    for matched in INVOKE_STANDALONE.find_iter(text) {
        if is_inside_code_fence(text, matched.start()) {
            continue;
        }
        for call in parse_invoke_body(matched.as_str()) {
            candidates.push(Candidate {
                name: call.name,
                arguments: call.arguments,
                grammar: "invoke",
                range: GrammarRange {
                    start: matched.start(),
                    end: matched.end(),
                },
                strip_only: false,
            });
        }
    }

    candidates.extend(parse_malformed_minimax_invoke(text));
    candidates
}

/// `parseInvokeBody` (`:693-704`).
fn parse_invoke_body(body: &str) -> Vec<ParsedCall> {
    let mut calls = Vec::new();

    for invoke in INVOKE_BODY.captures_iter(body) {
        let Some(name) = invoke.get(1).map(|name| name.as_str().trim()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        calls.push(ParsedCall {
            name: name.to_owned(),
            arguments: parse_invoke_arguments(
                invoke.get(2).map(|body| body.as_str()).unwrap_or(""),
            ),
        });
    }

    calls
}

/// `parseInvokeArguments` (`:706-719`): unlike the DSML variant, an untyped
/// parameter value goes through `maybeParseJsonValue`.
fn parse_invoke_arguments(body: &str) -> Map<String, JsonValue> {
    let mut args = Map::new();

    for param in INVOKE_PARAM.captures_iter(body) {
        let Some(key) = param.get(1).map(|key| key.as_str().trim()) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        let raw = param.get(3).map(|value| value.as_str()).unwrap_or("");
        let value = if param.get(2).map(|value| value.as_str()) == Some("false") {
            parse_json_value_or_string(raw.trim())
        } else {
            maybe_parse_json_value(raw.trim())
        };
        args.insert(key.to_owned(), value);
    }

    if !args.is_empty() {
        return args;
    }
    parse_json_object(body.trim())
        .and_then(|parsed| normalize_arguments_object(&JsonValue::Object(parsed)))
        .unwrap_or_default()
}

/// `parseMalformedMiniMaxInvoke` (`:721-740`): the angle brackets are gone
/// entirely (`invoke name="x"> … parameter name="y">val parameter>`).
fn parse_malformed_minimax_invoke(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for matched in MALFORMED_INVOKE.captures_iter(text) {
        let Some(whole) = matched.get(0) else {
            continue;
        };
        if is_inside_code_fence(text, whole.start()) {
            continue;
        }
        let start = whole.start() + usize::from(whole.as_str().starts_with('\n'));
        let Some(name) = matched.get(2).map(|name| name.as_str().trim()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut args = Map::new();
        let body = matched.get(3).map(|body| body.as_str()).unwrap_or("");
        for param in MALFORMED_PARAM.captures_iter(body) {
            let Some(key) = param.get(1).map(|key| key.as_str().trim()) else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            let raw = param
                .get(2)
                .map(|value| value.as_str())
                .unwrap_or("")
                .trim();
            args.insert(key.to_owned(), maybe_parse_json_value(raw));
        }
        candidates.push(Candidate {
            name: name.to_owned(),
            arguments: args,
            grammar: "invoke",
            range: GrammarRange {
                start,
                end: whole.end(),
            },
            strip_only: false,
        });
    }

    candidates
}

// ---------------------------------------------------------------------------
// C/G/H. `<tool_call>` wrappers (`:742-845`)
// ---------------------------------------------------------------------------

/// `parseToolCallXml` (`:742-785`): `<tool_call>`/`<tools>` wrappers whose
/// closer must carry the same tag name. TS expresses the pairing with a
/// backreference (`<\/\1>`), which the Rust regex crate does not support, so
/// the pairing is reproduced with an explicit close scan; `matchAll` advances
/// past each whole match, which [`Regex::captures_at`] with an explicit cursor
/// reproduces.
fn parse_tool_call_xml(text: &str, enabled: &BTreeSet<&'static str>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut cursor = 0usize;

    while let Some(captures) = TOOL_CALL_XML_OPEN.captures_at(text, cursor) {
        let (Some(open), Some(tag)) = (captures.get(0), captures.get(1)) else {
            break;
        };
        let tag = tag.as_str().to_ascii_lowercase();
        let close_pattern = match tag.as_str() {
            "tools" => &*CLOSE_TOOLS_TAG,
            _ => &*CLOSE_TOOL_CALL_TAG,
        };
        let Some(close) = close_pattern.find_at(text, open.end()) else {
            cursor = open.end();
            continue;
        };

        let (start, end) = (open.start(), close.end());
        let body = &text[open.end()..close.start()];
        cursor = end;

        if is_inside_code_fence(text, start) {
            continue;
        }
        let range = GrammarRange { start, end };

        if enabled.contains("granite") || enabled.contains("qwen") {
            let json_grammar = if tag == "tools" || !enabled.contains("granite") {
                "qwen"
            } else {
                "granite"
            };
            for call in parse_tool_call_json_body(body, json_grammar) {
                candidates.push(Candidate { range, ..call });
            }
        }

        if enabled.contains("glm") {
            if let Some(call) = parse_glm_tool_call_body(body) {
                candidates.push(Candidate {
                    name: call.name,
                    arguments: call.arguments,
                    grammar: "glm",
                    range,
                    strip_only: false,
                });
            }
        }

        if enabled.contains("qwen") {
            for call in parse_qwen_function_body(body) {
                candidates.push(Candidate {
                    name: call.name,
                    arguments: call.arguments,
                    grammar: "qwen",
                    range,
                    strip_only: false,
                });
            }
        }
    }

    if enabled.contains("qwen") {
        for matched in BARE_QWEN_FUNCTION.find_iter(text) {
            if is_inside_code_fence(text, matched.start()) {
                continue;
            }
            for call in parse_qwen_function_body(matched.as_str()) {
                candidates.push(Candidate {
                    name: call.name,
                    arguments: call.arguments,
                    grammar: "qwen",
                    range: GrammarRange {
                        start: matched.start(),
                        end: matched.end(),
                    },
                    strip_only: false,
                });
            }
        }
    }

    candidates
}

/// `parseQwenFunctionBody` (`:811-829`): the Qwen3-Coder `<function=…>` /
/// `<parameter=…>` shape.
fn parse_qwen_function_body(body: &str) -> Vec<ParsedCall> {
    let mut calls = Vec::new();

    for matched in QWEN_FUNCTION.captures_iter(body) {
        let Some(name) = matched.get(1).map(|name| name.as_str().trim()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut args = Map::new();
        let inner = matched.get(2).map(|inner| inner.as_str()).unwrap_or("");
        for param in QWEN_PARAM.captures_iter(inner) {
            let Some(key) = param.get(1).map(|key| key.as_str().trim()) else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            let raw = param
                .get(2)
                .map(|value| value.as_str())
                .unwrap_or("")
                .trim();
            args.insert(key.to_owned(), maybe_parse_json_value(raw));
        }
        calls.push(ParsedCall {
            name: name.to_owned(),
            arguments: args,
        });
    }

    calls
}

/// `parseGlmToolCallBody` (`:831-845`): `<tool_call>name` + `<arg_key>` /
/// `<arg_value>` pairs. The name is the first whitespace-separated token
/// before the first `arg_key` and must be a bare identifier.
fn parse_glm_tool_call_body(body: &str) -> Option<ParsedCall> {
    let keys: Vec<String> = GLM_ARG_KEY
        .captures_iter(body)
        .map(|key| {
            key.get(1)
                .map(|key| key.as_str().trim())
                .unwrap_or("")
                .to_owned()
        })
        .filter(|key| !key.is_empty())
        .collect();
    let values: Vec<String> = GLM_ARG_VALUE
        .captures_iter(body)
        .map(|value| {
            value
                .get(1)
                .map(|value| value.as_str().trim())
                .unwrap_or("")
                .to_owned()
        })
        .collect();

    let name_end = if keys.is_empty() {
        body.len()
    } else {
        GLM_ARG_KEY
            .find(body)
            .map(|matched| matched.start())
            .unwrap_or(body.len())
    };
    let head = body[..name_end].trim();
    let name = head.split_whitespace().next().unwrap_or("");
    if name.is_empty() || !IDENT_FULL.is_match(name) {
        return None;
    }

    let mut args = Map::new();
    for (index, key) in keys.iter().enumerate() {
        args.insert(
            key.clone(),
            maybe_parse_json_value(values.get(index).map(String::as_str).unwrap_or("")),
        );
    }
    Some(ParsedCall {
        name: name.to_owned(),
        arguments: args,
    })
}

/// `parseBarePythonicToolCalls` (`:908-925`): a plain `name(kw=val)` call on
/// its own line (Granite 3.3/4.0 H-Small).
fn parse_bare_pythonic_tool_calls(text: &str, grammar: &'static str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for matched in PYTHONIC_CALL_LINE.captures_iter(text) {
        let Some(whole) = matched.get(0) else {
            continue;
        };
        if is_inside_code_fence(text, whole.start()) {
            continue;
        }
        let start = whole.start() + usize::from(whole.as_str().starts_with('\n'));
        let Some(open_offset) = whole.as_str().rfind('(') else {
            continue;
        };
        let open_paren = whole.start() + open_offset;
        let Some(close_paren) = find_matching(text, open_paren, '(', ')') else {
            continue;
        };
        let line_start = text[..open_paren]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        // JS `text.slice(lineStart, match.index)` returns `""` whenever
        // `lineStart >= match.index` — which is the normal case here, because
        // the match either starts at 0 or at the newline the line starts
        // after. Reproduced explicitly: a bare `a..b` with `a > b` panics.
        let before_match = if line_start < whole.start() {
            &text[line_start..whole.start()]
        } else {
            ""
        };
        if !before_match.trim().is_empty() {
            continue;
        }
        if let Some(call) = parse_pythonic_calls(&text[start..close_paren + 1])
            .into_iter()
            .next()
        {
            candidates.push(Candidate {
                name: call.name,
                arguments: call.arguments,
                grammar,
                range: GrammarRange {
                    start,
                    end: close_paren + 1,
                },
                strip_only: false,
            });
        }
    }

    candidates
}

// ---------------------------------------------------------------------------
// J. OLMo and the shared pythonic parser (`:927-976`)
// ---------------------------------------------------------------------------

/// `parseOlmo` (`:927-937`): newline-delimited pythonic calls inside
/// `<function_calls>`.
fn parse_olmo(text: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for matched in OLMO_FUNCTION_CALLS.captures_iter(text) {
        let (Some(whole), Some(body)) = (matched.get(0), matched.get(1)) else {
            continue;
        };
        if is_inside_code_fence(text, whole.start()) {
            continue;
        }
        for call in parse_pythonic_calls(body.as_str()) {
            candidates.push(Candidate {
                name: call.name,
                arguments: call.arguments,
                grammar: "olmo",
                range: GrammarRange {
                    start: whole.start(),
                    end: whole.end(),
                },
                strip_only: false,
            });
        }
    }

    candidates
}

/// `parsePythonicCalls` (`:939-952`).
fn parse_pythonic_calls(text: &str) -> Vec<ParsedCall> {
    let mut calls = Vec::new();

    for matched in PYTHONIC_CALL_LINE.captures_iter(text) {
        let (Some(whole), Some(name)) = (matched.get(0), matched.get(1)) else {
            continue;
        };
        let Some(open_offset) = whole.as_str().rfind('(') else {
            continue;
        };
        let open_paren = whole.start() + open_offset;
        let Some(close_paren) = find_matching(text, open_paren, '(', ')') else {
            continue;
        };
        calls.push(ParsedCall {
            name: name.as_str().to_owned(),
            arguments: parse_keyword_arguments(&text[open_paren + 1..close_paren]),
        });
    }

    calls
}

/// `parseKeywordArguments` (`:954-964`).
fn parse_keyword_arguments(text: &str) -> Map<String, JsonValue> {
    let mut args = Map::new();

    for part in split_top_level(text, ',') {
        let Some(equals) = find_top_level_char(&part, '=') else {
            continue;
        };
        let key = part[..equals].trim();
        if !IDENT_FULL.is_match(key) {
            continue;
        }
        args.insert(
            key.to_owned(),
            parse_pythonish_value(part[equals + 1..].trim()),
        );
    }

    args
}

/// `parsePythonishValue` (`:966-976`): Python literals first (`True`/`False`/
/// `None`, a bare number, a quoted string with JS `\\x` unescaping), then a
/// word-boundary-rewritten JSON parse, then the raw string.
fn parse_pythonish_value(raw: &str) -> JsonValue {
    let trimmed = raw.trim();
    match trimmed {
        "True" => return JsonValue::Bool(true),
        "False" => return JsonValue::Bool(false),
        "None" => return JsonValue::Null,
        _ => {}
    }

    if PY_NUMBER.is_match(trimmed) {
        return match trimmed.parse::<f64>() {
            Ok(number) if number.is_finite() => number_json(number),
            // JS `Number()` would be `Infinity`, which JSON has no form for;
            // the TS value would serialize to `null`.
            _ => JsonValue::Null,
        };
    }

    let single_quoted = trimmed.starts_with('\'') && trimmed.ends_with('\'');
    let double_quoted = trimmed.starts_with('"') && trimmed.ends_with('"');
    if single_quoted || double_quoted {
        let inner = if trimmed.len() >= 2 {
            &trimmed[1..trimmed.len() - 1]
        } else {
            ""
        };
        return JsonValue::String(unescape_js_quoted(inner));
    }

    // Three chained `replace(/\bX\b/g, …)` calls, left to right.
    let with_true = PY_TRUE_WORD.replace_all(trimmed, "true");
    let with_false = PY_FALSE_WORD.replace_all(&with_true, "false");
    let rewritten = PY_NONE_WORD.replace_all(&with_false, "null");
    parse_json_value_or_string(&rewritten)
}

/// JS `str.replace(/\\(['"\\])/g, "$1")` (`:973`): drop the backslash before
/// `'`, `"` and `\`.
fn unescape_js_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '"' || next == '\'' || next == '\\' {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
            out.push('\\');
        } else {
            out.push(ch);
        }
    }
    out
}

/// The JSON number for a JS number, keeping integral values integral so the
/// serialized form matches `JSON.stringify(Number("20"))`.
fn number_json(number: f64) -> JsonValue {
    if number.fract() == 0.0 && number >= (i64::MIN as f64) && number <= (i64::MAX as f64) {
        return JsonValue::Number(serde_json::Number::from(number as i64));
    }
    match serde_json::Number::from_f64(number) {
        Some(number) => JsonValue::Number(number),
        None => JsonValue::Null,
    }
}

// ---------------------------------------------------------------------------
// Dispatch (`:296-322`) and selection (`:1043-1068`)
// ---------------------------------------------------------------------------

/// `parseToolGrammarCandidates` (`:296-322`): the family dispatch, in TS
/// order (the order only matters for the stable tie-break in
/// [`select_candidates`]). `enabled` is the configured family set.
fn parse_tool_grammar_candidates(text: &str, enabled: &BTreeSet<&'static str>) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();

    if enabled.contains("dsml") {
        candidates.extend(parse_dsml(text));
        candidates.extend(parse_dsml_dangling_markers(text));
    }
    if enabled.contains("kimi") {
        candidates.extend(parse_kimi(text));
    }
    if enabled.contains("qwen") || enabled.contains("granite") || enabled.contains("kimi") {
        candidates.extend(parse_orphan_tool_call_closers(text));
    }
    if enabled.contains("mistral") {
        candidates.extend(parse_mistral(text));
        candidates.extend(parse_bare_json_tool_calls(text, "mistral"));
    }
    if enabled.contains("minimax-text") {
        candidates.extend(parse_minimax_text01(text));
    }
    if enabled.contains("invoke") {
        candidates.extend(parse_invoke_xml(text));
    }
    if enabled.contains("qwen") || enabled.contains("glm") || enabled.contains("granite") {
        candidates.extend(parse_tool_call_xml(text, enabled));
    }
    if enabled.contains("granite") {
        candidates.extend(parse_bare_pythonic_tool_calls(text, "granite"));
    }
    if enabled.contains("llama") {
        candidates.extend(parse_llama_python_tag(text));
        candidates.extend(parse_bare_json_tool_calls(text, "llama"));
    }
    if enabled.contains("olmo") {
        candidates.extend(parse_olmo(text));
    }

    candidates.retain(|candidate| candidate.range.end > candidate.range.start);
    candidates
}

/// `rangesOverlap` (`:1061-1063`): strict overlap; touching ranges do not
/// overlap.
fn ranges_overlap(a: &GrammarRange, b: &GrammarRange) -> bool {
    a.start < b.end && b.start < a.end
}

/// `selectCandidates` (`:1043-1059`): sort by start, longer first, then keep a
/// candidate unless it overlaps an already-selected one. Identical ranges are
/// *not* duplicates (the DSML multi-invoke case keeps every call from one
/// block).
fn select_candidates(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then_with(|| b.range.len().cmp(&a.range.len()))
    });

    let mut selected: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        let duplicate = selected.iter().any(|existing| {
            let same_range = existing.range.start == candidate.range.start
                && existing.range.end == candidate.range.end;
            !same_range && ranges_overlap(&existing.range, &candidate.range)
        });
        if !duplicate {
            selected.push(candidate);
        }
    }

    selected
}

/// `recoverMangledToolName` (`:180-191`): open models append index tokens to
/// tool names (`bash_1_234456789` for `bash`). Only a suffix matching
/// `^[_-\d]+$` is cut, and the longest known tool wins so `read_file_1_2`
/// never collapses to `read`. Unknown names and names whose remainder carries
/// real characters are returned untouched.
fn recover_mangled_tool_name(name: &str, known_tools: &BTreeSet<String>) -> String {
    if known_tools.contains(name) {
        return name.to_owned();
    }

    let mut best: Option<&String> = None;
    for tool in known_tools {
        if tool.len() >= name.len() || !name.starts_with(tool.as_str()) {
            continue;
        }
        if !MANGLED_TOOL_SUFFIX.is_match(&name[tool.len()..]) {
            continue;
        }
        if best.is_none_or(|best| tool.len() > best.len()) {
            best = Some(tool);
        }
    }

    best.cloned().unwrap_or_else(|| name.to_owned())
}

/// `isAllowedTool` (`:1065-1068`): with `requireKnownTool`, a name must be in
/// a **non-empty** known set — an empty set recovers nothing.
fn is_allowed_tool(
    name: &str,
    config: &GrammarRepairConfig,
    known_tools: &BTreeSet<String>,
) -> bool {
    if !config.require_known_tool {
        return true;
    }
    !known_tools.is_empty() && known_tools.contains(name)
}

/// `removeRanges` (`:1070-1080`): splice out the sorted ranges, then collapse
/// whitespace the removal left behind and trim.
fn remove_ranges(text: &str, ranges: &[GrammarRange]) -> String {
    let mut sorted: Vec<&GrammarRange> = ranges.iter().collect();
    sorted.sort_by_key(|range| range.start);

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for range in sorted {
        if range.start > cursor {
            result.push_str(&text[cursor..range.start]);
        }
        cursor = cursor.max(range.end);
    }
    result.push_str(&text[cursor..]);

    let result = SPACE_BEFORE_NEWLINE.replace_all(&result, "\n");
    let result = EXCESS_NEWLINES.replace_all(&result, "\n\n");
    result.trim().to_owned()
}

// ---------------------------------------------------------------------------
// Recovered ids (`:1098-1100`)
// ---------------------------------------------------------------------------

/// Per-process id seed. TS uses `Date.now().toString(36)`; a monotonic counter
/// keeps the pass clock-free (pure) and collision-free across calls (the TS
/// millisecond stamp can repeat within one message).
static RECOVERED_CALL_ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// `tool_repair_<grammar>_<base36>_<i>` (`makeRecoveredToolCallId`, `:1098-1100`).
fn make_recovered_tool_call_id(grammar: &str, index: usize) -> String {
    let sequence = RECOVERED_CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "tool_repair_{}_{}_{}",
        sanitize_grammar_name(grammar),
        base36(sequence),
        index
    )
}

/// JS `grammar.replace(/[^a-z0-9]/gi, "_")`: every non-ASCII-alphanumeric
/// character becomes an underscore (`minimax-text` → `minimax_text`).
fn sanitize_grammar_name(grammar: &str) -> String {
    grammar
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

/// `Number.prototype.toString(36)`.
fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_else(|_| "0".to_owned())
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Compiled `leakModels` matchers, keyed by the configured pattern fragment.
///
/// The fragments are static config, but the gate runs on every assistant
/// message; compiling `(?i)<pattern>` per pattern per message (the pre-fix
/// shape) paid a full regex compile on the hot path. An entry is `None` when
/// the fragment does not compile, so a bad config is not retried per message
/// either.
static LEAK_MODEL_MATCHERS: LazyLock<Mutex<HashMap<String, Option<Arc<Regex>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bound on compiled matcher entries. The key is a configured pattern
/// fragment, so the live set is normally a handful; a pathological config (or
/// a session that repeatedly reloads settings) must not grow the process-wide
/// cache without bound. Clearing on overflow only costs the next message one
/// recompile per pattern.
const MAX_LEAK_MODEL_MATCHERS: usize = 64;

/// Compile (once per pattern) and return the leak-model matcher for one
/// configured fragment. `None` means the fragment does not compile; the
/// comparison is case-insensitive (`(?i)` prefix, exactly the old inline
/// compile). The accessor is public so a test can pin the reuse (pointer
/// identity) without reaching into the gate itself.
#[doc(hidden)]
pub fn leak_model_matcher(pattern: &str) -> Option<Arc<Regex>> {
    let mut cache = LEAK_MODEL_MATCHERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(compiled) = cache.get(pattern) {
        return compiled.clone();
    }
    if cache.len() >= MAX_LEAK_MODEL_MATCHERS {
        cache.clear();
    }
    let compiled = Regex::new(&format!("(?i){pattern}")).ok().map(Arc::new);
    cache.insert(pattern.to_owned(), compiled.clone());
    compiled
}

/// TS `resolveGrammarRepairForModel` (`:158-170`): the config is returned
/// unchanged while `enabled` is true; otherwise a model id matching any
/// `leakModels` fragment turns the pass on for this message. A missing/empty
/// model id or an uncompilable fragment never enables it.
///
/// This gate and the GLM token-leak gate (`/glm/i`,
/// [`has_grammar_leak_bug`](super::aliases::has_grammar_leak_bug)) are
/// deliberately not merged: a GLM model whose id does not match `leakModels`
/// still gets the token strip and no grammar pass (scope doc §6).
pub fn resolve_grammar_repair_for_model(
    config: &GrammarRepairConfig,
    model: Option<&str>,
) -> GrammarRepairConfig {
    if config.enabled {
        return config.clone();
    }
    if config.leak_models.is_empty() {
        return config.clone();
    }
    let Some(model_id) = model.filter(|model| !model.is_empty()) else {
        return config.clone();
    };

    let matched = config
        .leak_models
        .iter()
        .any(|pattern| leak_model_matcher(pattern).is_some_and(|regex| regex.is_match(model_id)));
    if !matched {
        return config.clone();
    }

    let mut resolved = config.clone();
    resolved.enabled = true;
    resolved
}

/// TS `parseToolGrammarLeaks` (`:285-294`): the family parsers as a pure text
/// extractor — candidates selected and stripped of `stripOnly` entries, with
/// no name mangling and no gates.
pub fn parse_tool_grammar_leaks(text: &str, grammars: &[&'static str]) -> Vec<RecoveredToolCall> {
    let enabled: BTreeSet<&'static str> = grammars.iter().copied().collect();
    select_candidates(parse_tool_grammar_candidates(text, &enabled))
        .into_iter()
        .filter(|candidate| !candidate.strip_only)
        .map(|candidate| RecoveredToolCall {
            name: candidate.name,
            arguments: candidate.arguments,
            grammar: candidate.grammar,
        })
        .collect()
}

/// TS `repairAssistantMessageGrammarLeaks` (`:193-283`): parse every
/// text/thinking part of the message, strip the leaked ranges, and — in
/// `recover` mode, when the message carries no native tool call, and when at
/// least one candidate survived the gates — append the recovered calls as
/// `ToolCall` blocks and set `stop_reason` to
/// [`ToolUse`](StopReason::ToolUse).
///
/// **Stop-reason guard (pir safety fix, not in the TS package).** The TS port
/// writes `stopReason = "toolUse"` unconditionally, but the repair hook runs
/// before the loop's terminal-reason dispatch: a promoted call would turn a
/// `length` turn into an executing `toolUse` turn (bypassing
/// `failToolCallsFromTruncatedMessage`) and an `error`/`aborted` turn into a
/// successful tool turn (bypassing the hard exit and the turn-retry class).
/// The recovered calls are still appended, but for those three reasons the
/// incoming reason is preserved.
///
/// The pass is a no-op while `config.enabled` is false; callers apply the
/// per-model gate through [`resolve_grammar_repair_for_model`] first. The role
/// and content-array guards of the TS entry (`:198`) collapse for
/// [`AssistantMessage`], which is always the assistant role with a block list.
///
/// Mutates `message` in place (the one conversion-path deviation, see the
/// module docs) and reports what it did.
pub fn repair_message_grammar_leaks(
    message: &mut AssistantMessage,
    config: &GrammarRepairConfig,
    known_tools: &BTreeSet<String>,
) -> GrammarRepairResult {
    if !config.enabled {
        return GrammarRepairResult::default();
    }

    let enabled: BTreeSet<&'static str> = config.grammars.iter().copied().collect();
    let existing_tool_calls = message
        .content
        .iter()
        .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
        .count();

    let mut recovered_calls: Vec<RecoveredToolCall> = Vec::new();
    let mut stripped_ranges = 0usize;
    let mut changed = false;

    for index in 0..message.content.len() {
        let text = match &message.content[index] {
            ContentBlock::Text(part) => part.text.clone(),
            ContentBlock::Thinking(part) => part.thinking.clone(),
            _ => continue,
        };

        let candidates: Vec<Candidate> =
            select_candidates(parse_tool_grammar_candidates(&text, &enabled))
                .into_iter()
                .map(|candidate| {
                    let name = recover_mangled_tool_name(&candidate.name, known_tools);
                    if name == candidate.name {
                        candidate
                    } else {
                        Candidate { name, ..candidate }
                    }
                })
                .filter(|candidate| {
                    candidate.strip_only || is_allowed_tool(&candidate.name, config, known_tools)
                })
                .collect();

        if candidates.is_empty() {
            continue;
        }

        stripped_ranges += candidates.len();
        changed = true;

        for candidate in &candidates {
            if candidate.strip_only {
                continue;
            }
            // Safety (`:226-238`): an empty argument object is almost always a
            // malformed fragment (`<tool_call>write</tool_call>`) that would
            // otherwise dispatch as a real call with missing required fields.
            if candidate.arguments.is_empty() {
                if config.debug {
                    crate::ext_diag_info!(
                        "[pi-tool-repair] grammar-repair skipping {}:{} (empty arguments, \
                         likely malformed grammar)",
                        candidate.grammar,
                        candidate.name
                    );
                }
                continue;
            }
            recovered_calls.push(RecoveredToolCall {
                name: candidate.name.clone(),
                arguments: candidate.arguments.clone(),
                grammar: candidate.grammar,
            });
        }

        let stripped = remove_ranges(
            &text,
            &candidates.iter().map(|c| c.range).collect::<Vec<_>>(),
        );
        match &mut message.content[index] {
            ContentBlock::Text(part) => part.text = stripped,
            ContentBlock::Thinking(part) => part.thinking = stripped,
            _ => {}
        }
    }

    // Safety (`:251`): never double-recover — an existing native toolCall
    // means strip-only, whatever the mode.
    let should_recover = config.mode == GrammarRepairMode::Recover
        && existing_tool_calls == 0
        && !recovered_calls.is_empty();

    if !changed && !should_recover {
        return GrammarRepairResult::default();
    }

    if should_recover {
        for (index, call) in recovered_calls.iter().enumerate() {
            message.content.push(ContentBlock::ToolCall(ToolCall {
                id: make_recovered_tool_call_id(call.grammar, index),
                name: call.name.clone(),
                arguments: call.arguments.clone().into_iter().collect(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }));
        }
        // Never raise `ToolUse` over a terminal reason the agent loop acts on:
        // `Length` must reach `failToolCallsFromTruncatedMessage` (whose error
        // results make the model re-issue), and `Error`/`Aborted` must reach
        // the hard-exit branch with their retry class intact. The recovered
        // calls stay on the message — a `Length` turn fails them as truncated,
        // an errored/aborted turn is dropped by the request transform — but the
        // provider's reason wins.
        if !matches!(
            message.stop_reason,
            StopReason::Length | StopReason::Error | StopReason::Aborted
        ) {
            message.stop_reason = StopReason::ToolUse;
        }
    }

    if config.debug {
        let recovered = if should_recover {
            recovered_calls
                .iter()
                .map(|call| format!("{}:{}", call.grammar, call.name))
                .collect::<Vec<String>>()
                .join(",")
        } else {
            String::new()
        };
        let recovered = if recovered.is_empty() {
            "none".to_owned()
        } else {
            recovered
        };
        let mode = match config.mode {
            GrammarRepairMode::Recover => "recover",
            GrammarRepairMode::Strip => "strip",
        };
        crate::ext_diag_info!(
            "[pi-tool-repair] grammar-repair mode={mode} stripped={stripped_ranges} \
             recovered={recovered}"
        );
    }

    GrammarRepairResult {
        changed: changed || should_recover,
        recovered_calls: if should_recover {
            recovered_calls
        } else {
            Vec::new()
        },
        stripped_ranges,
    }
}
