use super::*;

/// Live per-task progress callback: receives the running result snapshot
/// (upstream's per-task `onUpdate` with the partial `SingleResult`).
pub(super) type OnTaskUpdate = Arc<dyn Fn(&SingleResult) + Send + Sync>;

/// The tool's live partial-result emitter (upstream tool `onUpdate`).
pub(super) type EmitUpdate = Option<ToolUpdateFn>;

pub(super) fn emit_partial(
    emit: &EmitUpdate,
    builder: &DetailsBuilder,
    results: &[SingleResult],
    running_text: Option<&str>,
) {
    if let Some(emit) = emit {
        let text = running_text.map(str::to_owned).unwrap_or_else(|| {
            results
                .last()
                .and_then(|result| get_final_output(&result.messages))
                .unwrap_or_else(|| "(running...)".to_owned())
        });
        emit(serde_json::json!({
            "content": [{ "type": "text", "text": text }],
            "details": builder.build(results),
        }));
    }
}

// ---------------------------------------------------------------------------
// Child execution (upstream `runSingleAgent`)
// ---------------------------------------------------------------------------

/// Resolve an agent `model` spec against the parent's provider registry
/// (upstream passes `--model provider/id` to the child CLI).
///
/// An explicit `provider/id` spec is resolved strictly within that provider
/// (upstream hands `provider/id` to the child CLI verbatim, which looks it up
/// in that provider only) — no cross-provider fallback, which could silently
/// route the child to a different provider's model that happens to share the
/// id. Only a bare single-segment id does the global search.
pub(super) fn resolve_model(
    registry: &crate::ai::provider::ProviderRegistry,
    spec: &str,
) -> Option<Model> {
    if let Some((provider, id)) = spec.split_once('/') {
        return registry.get_model(provider, id);
    }
    registry.get_models().into_iter().find(|m| m.id == spec)
}

/// The child's tool registry: the parent's base registry filtered by the
/// agent's `tools` allowlist; empty/absent = the pir default set (the parent's
/// base registry, unfiltered).
pub(super) fn child_tool_registry(env: &ToolSessionEnv, agent: &AgentConfig) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    match agent.tools.as_deref() {
        Some(tools) if !tools.is_empty() => {
            for name in tools {
                if let Some(tool) = env.base_tools.get(name) {
                    registry.register(tool);
                }
            }
        }
        _ => registry = (*env.base_tools).clone(),
    }
    registry
}

pub(super) fn ok_with_details(content: String, details: JsonValue) -> ToolResult {
    ToolResult {
        content,
        images: Vec::new(),
        details: Some(details),
        structured_content: None,
        is_error: false,
        terminate: false,
    }
}

pub(super) fn error_with_details(content: String, details: JsonValue) -> ToolResult {
    ToolResult {
        content,
        images: Vec::new(),
        details: Some(details),
        structured_content: None,
        is_error: true,
        terminate: false,
    }
}
