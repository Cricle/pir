//! Independent, host-neutral core of the `codemode` builtin.
//!
//! Codemode's execution half is a QuickJS sandbox whose only capability is the
//! nested-tool-call bridge; that runtime, its worker threads and the tool
//! registry stay host-side. This crate owns the pure, testable parts: script
//! source normalization (identifier-safe names derived from a script id) and
//! the `truncate_to_budget` helper the sandbox uses for bounded output.
//!
//! It depends on `pir-plugin-api` and third-party crates only — never on
//! `pir-core`.

#![deny(unsafe_code)]

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// The registered codemode tool name.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

// The inline-budget DEFAULT lives with the engine's settings
// (`pir_core::plugins::codemode::DEFAULT_INLINE_BUDGET`), because it is a
// settings value in TOKENS (3000) while this crate's truncation takes a token
// count. A second constant here would have to restate the unit and silently
// disagree with the settings default.

/// Turn an arbitrary script id into a valid JavaScript identifier.
///
/// Every byte outside `[A-Za-z0-9_$]` becomes `_`; a leading digit is prefixed
/// with `_` so the result is always a legal identifier.
pub fn to_codemode_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 1);
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '$' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return "_".to_owned();
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Characters per estimated token (upstream `CHARS_PER_TOKEN`).
pub const CHARS_PER_TOKEN: usize = 4;

/// Apply the token budget with the upstream `truncateOutput` layout: the
/// output keeps the START and END of the text (head `budget/2` + tail
/// `budget/2`), so a trailing return value or console block survives
/// truncation. The notice text is the upstream one, verbatim.
pub fn truncate_to_budget(output: &str, budget_tokens: u32) -> String {
    let max_chars = (budget_tokens as usize)
        .saturating_mul(CHARS_PER_TOKEN)
        .max(1);
    if output.len() <= max_chars {
        return output.to_owned();
    }
    let head_chars = max_chars / 2;
    let tail_chars = max_chars - head_chars;
    let mut head_end = head_chars;
    while head_end > 0 && !output.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = output.len().saturating_sub(tail_chars);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start < head_end {
        tail_start = head_end;
    }
    let removed = output.len() - head_end - (output.len() - tail_start);
    format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}…{} tokens truncated…{}",
        output.len().div_ceil(CHARS_PER_TOKEN),
        output.lines().count(),
        &output[..head_end],
        removed.div_ceil(CHARS_PER_TOKEN),
        &output[tail_start..]
    )
}

/// Minimal protocol-facing shell for the codemode builtin.
pub struct CodemodePlugin {
    info: PluginInfo,
}

impl CodemodePlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(PLUGIN_NAME),
        }
    }
}

impl BuiltinPlugin for CodemodePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = CodemodePlugin,
    name = "codemode",
    factory = new_codemode_plugin,
}
