// The embedded catalog resources (kept in one place so `include_str!`
// paths are relative to this file).

/// Look up an embedded catalog file body by file name.
pub fn embedded_catalog(file: &str) -> crate::ai::error::AiResult<&'static str> {
    match file {
        "amazon-bedrock.json" => Ok(include_str!("amazon-bedrock.json")),
        "anthropic.json" => Ok(include_str!("anthropic.json")),
        "ant-ling.json" => Ok(include_str!("ant-ling.json")),
        "azure.json" => Ok(include_str!("azure.json")),
        "baseten.json" => Ok(include_str!("baseten.json")),
        "cerebras.json" => Ok(include_str!("cerebras.json")),
        "cloudflare-ai-gateway.json" => Ok(include_str!("cloudflare-ai-gateway.json")),
        "cloudflare-workers-ai.json" => Ok(include_str!("cloudflare-workers-ai.json")),
        "deepseek.json" => Ok(include_str!("deepseek.json")),
        "fireworks.json" => Ok(include_str!("fireworks.json")),
        "github-copilot.json" => Ok(include_str!("github-copilot.json")),
        "google.json" => Ok(include_str!("google.json")),
        "google-vertex.json" => Ok(include_str!("google-vertex.json")),
        "groq.json" => Ok(include_str!("groq.json")),
        "huggingface.json" => Ok(include_str!("huggingface.json")),
        "kimi-coding.json" => Ok(include_str!("kimi-coding.json")),
        "meta.json" => Ok(include_str!("meta.json")),
        "minimax.json" => Ok(include_str!("minimax.json")),
        "minimax-cn.json" => Ok(include_str!("minimax-cn.json")),
        "mistral.json" => Ok(include_str!("mistral.json")),
        "moonshotai.json" => Ok(include_str!("moonshotai.json")),
        "moonshotai-cn.json" => Ok(include_str!("moonshotai-cn.json")),
        "nvidia.json" => Ok(include_str!("nvidia.json")),
        "openai.json" => Ok(include_str!("openai.json")),
        "openai-codex.json" => Ok(include_str!("openai-codex.json")),
        "opencode.json" => Ok(include_str!("opencode.json")),
        "opencode-go.json" => Ok(include_str!("opencode-go.json")),
        "openrouter.json" => Ok(include_str!("openrouter.json")),
        "qwen-token-plan.json" => Ok(include_str!("qwen-token-plan.json")),
        "qwen-token-plan-cn.json" => Ok(include_str!("qwen-token-plan-cn.json")),
        "qwen-token-plan-individual.json" => Ok(include_str!("qwen-token-plan-individual.json")),
        "radius.json" => Ok(include_str!("radius.json")),
        "together.json" => Ok(include_str!("together.json")),
        "typesafe.json" => Ok(include_str!("typesafe.json")),
        "vercel-ai-gateway.json" => Ok(include_str!("vercel-ai-gateway.json")),
        "xai.json" => Ok(include_str!("xai.json")),
        "xiaomi.json" => Ok(include_str!("xiaomi.json")),
        "xiaomi-token-plan-ams.json" => Ok(include_str!("xiaomi-token-plan-ams.json")),
        "xiaomi-token-plan-cn.json" => Ok(include_str!("xiaomi-token-plan-cn.json")),
        "xiaomi-token-plan-sgp.json" => Ok(include_str!("xiaomi-token-plan-sgp.json")),
        "zai.json" => Ok(include_str!("zai.json")),
        "zai-coding-cn.json" => Ok(include_str!("zai-coding-cn.json")),
        other => Err(crate::ai::error::AiError::Authentication(format!(
            "unknown catalog resource: {other}"
        ))),
    }
}
