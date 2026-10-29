// ---------------------------------------------------------------------------
// Tool execution defaults (pir-only)
// ---------------------------------------------------------------------------

/// Default foreground shell-command timeout when `tools.commandTimeoutMs` is
/// absent (120 s). This is a deliberate deviation from upstream's
/// no-default-timeout semantics: a harness that kills a run at its own ceiling
/// otherwise loses the whole run to a single hung command. An explicit
/// per-call `timeout` always wins, and `tools.commandTimeoutMs: 0` restores
/// upstream behavior exactly.
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 120_000;

// ---------------------------------------------------------------------------
// Tool list helpers (upstream settings-manager.ts:86-138)
// ---------------------------------------------------------------------------

/// Whether a tool selection entry is a `+name` or `-name` modifier.
pub fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// Tools enabled at startup when `defaultTools` does not change them
/// (upstream `DEFAULT_TOOL_NAMES`).
pub const DEFAULT_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// Validate a tool list from `--tools` or the settings `defaultTools`. It is
/// either an allowlist of plain names/patterns or a list of only
/// `+name`/`-name` entries with exact names. Returns the problem, or `None`
/// when valid (upstream `getToolListError`).
pub fn get_tool_list_error(entries: &[String]) -> Option<String> {
    let modifiers: Vec<&String> = entries
        .iter()
        .filter(|entry| is_tool_modifier(entry))
        .collect();
    if modifiers.is_empty() {
        return None;
    }
    if modifiers.len() < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_string());
    }
    if let Some(pattern) = modifiers.iter().find(|entry| entry.contains('*')) {
        return Some(format!(
            "+name and -name entries take exact tool names, not patterns: {pattern}"
        ));
    }
    None
}

/// Apply the `+name` and `-name` entries of `entries` to `base` in order:
/// `+name` adds a tool and `-name` removes one (upstream `applyToolModifiers`).
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries {
        if !is_tool_modifier(entry) {
            continue;
        }
        let name = &entry[1..];
        let index = tools.iter().position(|tool| tool == name);
        match (entry.starts_with('+'), index) {
            (true, None) if !name.is_empty() => tools.push(name.to_string()),
            (false, Some(index)) => {
                tools.remove(index);
            }
            _ => {}
        }
    }
    tools
}

/// Merge the `defaultTools` lists of two settings layers. A list with plain
/// tool names replaces the inherited one; a list of only `+name`/`-name`
/// entries is appended, so it modifies the inherited selection (upstream
/// `mergeDefaultTools`).
pub(super) fn merge_default_tools(
    base: Option<&Vec<String>>,
    over: Option<Vec<String>>,
) -> Option<Vec<String>> {
    let over = over?;
    let Some(base) = base else {
        return Some(over);
    };
    if !over.iter().all(|entry| is_tool_modifier(entry)) {
        return Some(over);
    }
    let mut merged = base.clone();
    merged.extend(over);
    Some(merged)
}

/// Resolve a merged `defaultTools` list: plain names replace
/// [`DEFAULT_TOOL_NAMES`], then `+name` adds and `-name` removes a tool, in
/// list order (upstream `resolveDefaultTools`).
pub fn resolve_default_tools(entries: &[String]) -> Vec<String> {
    let plain: Vec<String> = entries
        .iter()
        .filter(|entry| !is_tool_modifier(entry))
        .cloned()
        .collect();
    let base: Vec<String> = if !plain.is_empty() || entries.is_empty() {
        plain
    } else {
        DEFAULT_TOOL_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect()
    };
    apply_tool_modifiers(&base, entries)
}

// ---------------------------------------------------------------------------
