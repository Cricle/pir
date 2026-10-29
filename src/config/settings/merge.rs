use super::*;

// Deep merge
// ---------------------------------------------------------------------------

/// Deep merge `overrides` into `base` (project wins); nested objects merge
/// recursively, arrays and scalars are replaced (upstream
/// `deepMergeSettings`), with the `defaultTools` modifier rule.
pub fn deep_merge_settings_values(base: &mut Value, overrides: &Value) {
    if let (Some(base_map), Some(over_map)) = (base.as_object_mut(), overrides.as_object()) {
        // Snapshot the inherited `defaultTools` before the generic replace so
        // the modifier rule can append to it.
        let base_tools: Vec<String> = base_map
            .get("defaultTools")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for (key, value) in over_map {
            match base_map.get_mut(key) {
                Some(existing) if existing.is_object() && value.is_object() => {
                    deep_merge_settings_values(existing, value);
                }
                _ => {
                    base_map.insert(key.clone(), value.clone());
                }
            }
        }
        // `defaultTools` special rule: a pure-modifier override list appends to
        // the inherited list instead of replacing it. An empty list is a pure
        // modifier list (upstream `[].every(isModifier)` is true), so it keeps
        // the inherited selection too.
        if let Some(over_tools) = overrides.get("defaultTools").and_then(Value::as_array) {
            let over_tools: Vec<String> = over_tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if over_tools.iter().all(|entry| is_tool_modifier(entry)) {
                let merged = merge_default_tools(Some(&base_tools), Some(over_tools));
                if let Some(merged) = merged {
                    base["defaultTools"] = serde_json::to_value(merged).unwrap_or(Value::Null);
                }
            }
        }
        return;
    }
    *base = overrides.clone();
}

// ---------------------------------------------------------------------------
// Migrations (upstream settings-manager.ts:394-454)
// ---------------------------------------------------------------------------

/// Migrate legacy settings formats in place, before typed parsing:
/// `queueMode` -> `steeringMode`; `websockets: boolean` -> `transport`;
/// legacy `skills: {enableSkillCommands, customDirectories}` -> `skills:
/// string[]` (with `enableSkillCommands` hoisted);
/// `retry.maxDelayMs` -> `retry.provider.maxRetryDelayMs`.
pub fn migrate_settings(settings: &mut Value) {
    let Some(map) = settings.as_object_mut() else {
        return;
    };

    // queueMode -> steeringMode
    if map.contains_key("queueMode") && !map.contains_key("steeringMode") {
        let queue_mode = map.remove("queueMode");
        if let Some(value) = queue_mode {
            map.insert("steeringMode".to_string(), value);
        }
    }

    // websockets: boolean -> transport
    if !map.contains_key("transport") {
        match map.get("websockets") {
            Some(Value::Bool(true)) => {
                map.insert("transport".to_string(), Value::String("websocket".into()));
                map.remove("websockets");
            }
            Some(Value::Bool(false)) => {
                map.insert("transport".to_string(), Value::String("sse".into()));
                map.remove("websockets");
            }
            _ => {}
        }
    }

    // Legacy skills object -> array
    if let Some(Value::Object(skills)) = map.get("skills") {
        let enable = skills.get("enableSkillCommands").cloned();
        let custom = skills.get("customDirectories").cloned();
        if let Some(enable) = enable {
            map.entry("enableSkillCommands".to_string())
                .or_insert(enable);
        }
        match custom {
            Some(Value::Array(dirs)) if !dirs.is_empty() => {
                map.insert("skills".to_string(), Value::Array(dirs));
            }
            _ => {
                map.remove("skills");
            }
        }
    }

    // retry.maxDelayMs -> retry.provider.maxRetryDelayMs
    if let Some(Value::Object(retry)) = map.get_mut("retry") {
        let max_delay = retry.remove("maxDelayMs");
        if let Some(Value::Number(delay)) = max_delay {
            let provider = retry
                .entry("provider".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(provider_map) = provider.as_object_mut() {
                provider_map
                    .entry("maxRetryDelayMs".to_string())
                    .or_insert(Value::Number(delay));
            }
        }
    }
}
