//! Login interaction contracts (upstream `AuthInteraction` in
//! `packages/ai/src/auth/types.ts`): the prompt/notify surface the flows use,
//! plus the app-supplied `LoginOptions`.

use std::future::Future;
use std::pin::Pin;

use crate::ai::error::AiResult;

/// Events a login flow emits for display (upstream `AuthEvent`).
#[derive(Debug, Clone)]
pub enum AuthEvent {
    /// Ambient information with optional links.
    Info {
        message: String,
        links: Vec<AuthInfoLink>,
    },
    /// A URL the user must open (browser sign-in).
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    /// Device-code sign-in instructions.
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
    },
    /// Progress text.
    Progress { message: String },
}

/// A link attached to an `AuthEvent::Info`.
#[derive(Debug, Clone)]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

/// A prompt shown to the user during login (upstream `AuthPrompt`).
#[derive(Debug, Clone)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<SelectOption>,
    },
    /// Paste a code or redirect URL while the callback server is also waiting.
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
}

/// One option of an `AuthPrompt::Select`.
#[derive(Debug, Clone)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// Interaction callbacks serving both api-key and OAuth login flows
/// (upstream `AuthInteraction`).
pub trait AuthInteraction: Send + Sync {
    /// Prompt for input; the returned string is the entered text or the
    /// selected option id. Errors mean the user cancelled.
    fn prompt<'a>(
        &'a self,
        prompt: AuthPrompt,
    ) -> Pin<Box<dyn Future<Output = AiResult<String>> + Send + 'a>>;

    /// Emit a display event.
    fn notify(&self, event: AuthEvent);
}

/// App-supplied login context (upstream `LoginOptions`).
#[derive(Clone, Default)]
pub struct LoginOptions {
    /// Returns the stable installation id (e.g. OpenAI's agent host id).
    /// Created on first use and stable afterwards.
    pub get_device_id: Option<std::sync::Arc<dyn Fn() -> String + Send + Sync>>,
    /// The name the agent introduces itself with (defaults to pi's own name).
    pub agent_name: Option<String>,
}

impl std::fmt::Debug for LoginOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginOptions")
            .field("get_device_id", &self.get_device_id.is_some())
            .field("agent_name", &self.agent_name)
            .finish()
    }
}

impl LoginOptions {
    /// Resolve the device id, erroring like upstream when the app did not
    /// supply `getDeviceId` but the flow needs it.
    pub fn device_id(&self) -> crate::ai::error::AiResult<String> {
        self.get_device_id.as_ref().map(|get| get()).ok_or_else(|| {
            crate::ai::error::AiError::Authentication(
                "Sign in with ChatGPT requires a device ID (UUID) for this installation"
                    .to_string(),
            )
        })
    }
}
