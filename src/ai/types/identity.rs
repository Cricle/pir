//! Client identity: `User-Agent` and provider attribution headers.
//!
//! Rust equivalent of upstream `packages/ai/src/utils/identity.ts`. A request
//! can carry a [`ClientIdentity`] (per request via
//! [`StreamOptions::identity`](super::StreamOptions::identity), or as the
//! registry-wide default); the identity decides the outgoing `User-Agent` and
//! the provider-specific attribution headers (OpenRouter / NVIDIA NIM /
//! OpenCode).

use std::collections::BTreeMap;

use super::stream::ProviderHeaders;

/// The pi-ai version embedded in a detailed `User-Agent`
/// (upstream `PI_AI_VERSION`).
pub const PI_AI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How a request identifies itself in `User-Agent` (upstream
/// `UserAgentMode`): `"short"` → `pi`, `"detailed"` →
/// `pi/<version> (<platform>; <runtime>/<version>; <arch>)`, or a custom
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAgentMode {
    Short,
    Detailed,
    Custom(String),
}

impl UserAgentMode {
    /// The wire value: `"short"`, `"detailed"`, or `{"custom": "..."}`.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            UserAgentMode::Short => serde_json::Value::String("short".to_string()),
            UserAgentMode::Detailed => serde_json::Value::String("detailed".to_string()),
            UserAgentMode::Custom(custom) => {
                serde_json::json!({ "custom": custom })
            }
        }
    }

    /// Parse the wire shape. `None` for anything unrecognized.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::String(mode) if mode == "short" => Some(Self::Short),
            serde_json::Value::String(mode) if mode == "detailed" => Some(Self::Detailed),
            serde_json::Value::Object(object) => object
                .get("custom")
                .and_then(serde_json::Value::as_str)
                .map(|custom| Self::Custom(custom.to_string())),
            _ => None,
        }
    }
}

/// Who requests are attributed to (upstream `ClientAttribution`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientAttribution {
    /// App name: OpenRouter app title, NVIDIA invoke origin, OpenCode client.
    pub name: String,
    /// App URL: OpenRouter `HTTP-Referer`.
    pub url: String,
    /// App categories (OpenRouter `X-OpenRouter-Categories`).
    pub categories: Vec<String>,
}

/// pi's own attribution (upstream `PI_ATTRIBUTION`).
pub fn pi_attribution() -> ClientAttribution {
    ClientAttribution {
        name: "pi".to_string(),
        url: "https://pi.dev".to_string(),
        categories: vec!["cli-agent".to_string()],
    }
}

/// How pi-ai identifies the calling app to providers (upstream
/// `ClientIdentity`). Every field is optional; absent fields use defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientIdentity {
    /// Default: [`UserAgentMode::Detailed`].
    pub user_agent: Option<UserAgentMode>,
    /// Provider attribution headers. `Some(None)` is the upstream explicit
    /// `null`: it turns off attribution inherited from a merged identity.
    pub attribution: Option<Option<ClientAttribution>>,
    /// Version in the detailed `User-Agent`. Default: [`PI_AI_VERSION`].
    pub version: Option<String>,
}

impl ClientIdentity {
    /// An identity that sends a short `User-Agent: pi` and no attribution.
    pub fn short() -> Self {
        Self {
            user_agent: Some(UserAgentMode::Short),
            attribution: Some(None),
            version: None,
        }
    }

    /// An identity with pi attribution.
    pub fn with_pi_attribution() -> Self {
        Self {
            user_agent: None,
            attribution: Some(Some(pi_attribution())),
            version: None,
        }
    }

    /// Merge two identities field by field; `over` wins and `None` fields are
    /// skipped (upstream `mergeClientIdentity`).
    pub fn merge(base: Option<&ClientIdentity>, over: Option<&ClientIdentity>) -> Option<Self> {
        let mut merged = base.cloned();
        if let Some(over) = over {
            let target = merged.get_or_insert_with(ClientIdentity::default);
            if over.user_agent.is_some() {
                target.user_agent = over.user_agent.clone();
            }
            if over.attribution.is_some() {
                target.attribution = over.attribution.clone();
            }
            if over.version.is_some() {
                target.version = over.version.clone();
            }
        }
        merged
    }
}

/// The version in a detailed `User-Agent` (upstream `getClientVersion`).
pub fn client_version(identity: Option<&ClientIdentity>) -> &str {
    identity
        .and_then(|identity| identity.version.as_deref())
        .unwrap_or(PI_AI_VERSION)
}

/// Format the `User-Agent` value for an identity (upstream
/// `formatUserAgent`).
pub fn format_user_agent(identity: Option<&ClientIdentity>) -> String {
    let mode = identity
        .and_then(|identity| identity.user_agent.as_ref())
        .unwrap_or(&UserAgentMode::Detailed);
    match mode {
        UserAgentMode::Custom(custom) => custom.clone(),
        UserAgentMode::Short => "pi".to_string(),
        UserAgentMode::Detailed => format!(
            "pi/{} ({}; rust; {})",
            client_version(identity),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    }
}

/// Hostname equality against a request base URL (upstream `matchesHost`).
fn matches_host(base_url: &str, expected_host: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host == expected_host))
        .unwrap_or(false)
}

fn is_openrouter(provider: &str, base_url: &str) -> bool {
    provider == "openrouter" || base_url.contains("openrouter.ai")
}

fn is_nvidia_nim(provider: &str, base_url: &str) -> bool {
    provider == "nvidia" || matches_host(base_url, "integrate.api.nvidia.com")
}

fn is_opencode(provider: &str, base_url: &str) -> bool {
    provider == "opencode" || provider == "opencode-go" || matches_host(base_url, "opencode.ai")
}

/// Provider attribution headers of an identity for a request target (upstream
/// `getAttributionHeaders`). Empty when attribution is off.
pub fn attribution_headers(
    provider: &str,
    base_url: &str,
    identity: Option<&ClientIdentity>,
) -> ProviderHeaders {
    let attribution = match identity.and_then(|identity| identity.attribution.as_ref()) {
        Some(Some(attribution)) => attribution,
        _ => return ProviderHeaders::new(),
    };
    let mut headers = ProviderHeaders::new();
    if is_openrouter(provider, base_url) {
        headers.insert("HTTP-Referer".to_string(), attribution.url.clone());
        headers.insert("X-OpenRouter-Title".to_string(), attribution.name.clone());
        if !attribution.categories.is_empty() {
            headers.insert(
                "X-OpenRouter-Categories".to_string(),
                attribution.categories.join(","),
            );
        }
        return headers;
    }
    if is_nvidia_nim(provider, base_url) {
        let mut chars = attribution.name.chars();
        let name = match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        };
        headers.insert("X-BILLING-INVOKE-ORIGIN".to_string(), name);
        return headers;
    }
    if is_opencode(provider, base_url) {
        headers.insert("x-opencode-client".to_string(), attribution.name.clone());
    }
    headers
}

/// The `User-Agent` plus provider attribution an identity adds to a request
/// (upstream `getIdentityHeaders`). Callers merge these BELOW model/provider
/// and request headers, so configured headers override them and an empty
/// (suppressed) header removes them.
pub fn identity_headers(
    provider: &str,
    base_url: &str,
    identity: Option<&ClientIdentity>,
) -> ProviderHeaders {
    let mut headers = BTreeMap::from([("User-Agent".to_string(), format_user_agent(identity))]);
    headers.extend(attribution_headers(provider, base_url, identity));
    headers
}
