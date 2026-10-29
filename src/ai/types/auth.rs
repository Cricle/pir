//! Credential and auth-relevant types.

use std::fmt;

use super::stream::{ProviderEnv, ProviderHeaders};

// ---------------------------------------------------------------------------
// Auth types
// ---------------------------------------------------------------------------

/// Resolved auth for a single model request.
#[derive(Clone, Default)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub base_url: Option<String>,
}

/// Redacting `Debug`: resolved request auth carries the api key (or an OAuth
/// access token mapped into `api_key`) plus provider headers that can include
/// authorization values. Only the header names are shown.
impl fmt::Debug for ModelAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field(
                "header_keys",
                &self
                    .headers
                    .as_ref()
                    .map(|headers| headers.keys().collect::<Vec<_>>()),
            )
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// A stored API key credential.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyCredential {
    #[serde(rename = "type")]
    pub credential_type: CredentialType,
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// Redacting `Debug`: the key (and any env values, which can hold secrets) is
/// replaced by a marker so a credential never leaks through logs or panics.
impl fmt::Debug for ApiKeyCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyCredential")
            .field("credential_type", &self.credential_type)
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field(
                "env_keys",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .finish()
    }
}

impl ApiKeyCredential {
    pub fn new(key: Option<String>) -> Self {
        Self {
            credential_type: CredentialType::ApiKey,
            key,
            env: None,
        }
    }
}

/// Credential type tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CredentialType {
    #[serde(rename = "api_key")]
    ApiKey,
}

/// A stored credential. Only api-key credentials remain: OAuth flows were
/// removed by scope decision (custom base URL + auth only).
///
/// Wire shape: the payload object itself, `{"type": "api_key", ...}`, with one
/// `type` key. The derive cannot express it here: [`ApiKeyCredential`] renames
/// its own `credential_type` field to `type`, so `#[serde(tag = "type")]` wrote
/// the key twice and, on the way back in, stripped the tag before deserializing
/// the payload — leaving the payload's `type` field missing (the same defect as
/// the `role` tag of `crate::ai::types::Message`). `auth.json` is not affected
/// either way: `crate::ai::auth::storage::StoredCredential` owns that file's
/// shape with its own impls.
#[derive(Debug, Clone)]
pub enum Credential {
    ApiKey(ApiKeyCredential),
}

impl serde::Serialize for Credential {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // The payload owns the `type` key, so serializing it is the flat
        // `{"type": "api_key", ...}` shape with exactly one `type` key.
        let Credential::ApiKey(payload) = self;
        payload.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Credential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        use serde_json::from_value;

        // `type` is both the variant tag and the payload's own field, so the
        // whole object is buffered and handed to the payload.
        let mut object = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Object(object) => object,
            _ => return Err(D::Error::custom("credential must be a JSON object")),
        };
        let credential_type = match object.get("type") {
            Some(serde_json::Value::String(tag)) => tag.clone(),
            Some(_) => return Err(D::Error::custom("credential `type` must be a string")),
            None => return Err(D::Error::missing_field("type")),
        };
        // The tag is authoritative and is written back into the payload.
        object.insert(
            "type".to_owned(),
            serde_json::Value::String(credential_type.clone()),
        );
        let payload = serde_json::Value::Object(object);
        let parsed = match credential_type.as_str() {
            "api_key" => from_value::<ApiKeyCredential>(payload).map(Credential::ApiKey),
            other => return Err(D::Error::unknown_variant(other, &["api_key"])),
        };
        parsed.map_err(D::Error::custom)
    }
}

/// Result of resolving auth for a model.
#[derive(Debug, Clone)]
pub struct AuthResult {
    pub auth: ModelAuth,
    /// Provider-scoped environment/config values.
    pub env: Option<ProviderEnv>,
    /// Human-readable label for status UI.
    pub source: Option<String>,
}

/// Auth check result.
#[derive(Debug, Clone)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub auth_type: AuthType,
}

/// Auth type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    ApiKey,
    OAuth,
}
