//! Provider OAuth flows.
//!
//! Rust equivalents of upstream `packages/ai/src/auth/oauth/*.ts`. Each flow
//! exposes authorize/token constants copied verbatim from upstream plus the
//! [`OAuthFlow`] contract (`login`/`refresh`/`toAuth`).

pub mod anthropic;
pub mod github_copilot;
pub mod kimi_coding;
pub mod meta;
pub mod openai_chatgpt;
pub mod openai_codex;
pub mod openrouter;
pub mod radius;
pub mod xai;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::interaction::{AuthInteraction, LoginOptions};
use crate::ai::error::AiResult;
use crate::ai::types::ModelAuth;

/// A stored canonical OAuth credential (upstream `OAuthCredential`): the
/// `auth.json` entry shape is `{ "type": "oauth", refresh, access, expires }`
/// plus flow-specific extra fields flattened alongside (`accountId`,
/// `availableModelIds`, `enterpriseUrl`, `clientId`, `scopes`, `scope`,
/// `gatewayConfig`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    /// Expiry in epoch milliseconds.
    pub expires: i64,
    /// Flow-specific extra fields, preserved verbatim on the wire.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Redacting `Debug`: tokens must never reach logs, panic payloads, or debug
/// dumps. The flattened `extra` map can carry flow-specific token fields, so
/// only its keys are shown.
impl fmt::Debug for OAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("refresh", &"[REDACTED]")
            .field("access", &"[REDACTED]")
            .field("expires", &self.expires)
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl OAuthCredential {
    /// Build a credential with no extra fields.
    pub fn new(access: impl Into<String>, refresh: impl Into<String>, expires: i64) -> Self {
        Self {
            refresh: refresh.into(),
            access: access.into(),
            expires,
            extra: BTreeMap::new(),
        }
    }

    /// Read an extra string field (e.g. `accountId`).
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }

    /// Set or remove an extra string field.
    pub fn set_extra(&mut self, key: &str, value: Option<Value>) {
        match value {
            Some(value) => {
                self.extra.insert(key.to_string(), value);
            }
            None => {
                self.extra.remove(key);
            }
        }
    }
}

/// The OAuth contract (upstream `OAuthAuth`): `login` performs the interactive
/// flow, `refresh` exchanges the refresh token (network), and `toAuth` derives
/// request auth from a stored credential.
pub trait OAuthFlow: Send + Sync {
    /// Display name, e.g. "Anthropic (Claude Pro/Max)".
    fn name(&self) -> &str;

    /// Whether access through this auth method is subscription-backed.
    fn is_subscription(&self) -> bool {
        false
    }

    /// Selector label for the login option, e.g. "Sign in with ChatGPT".
    fn login_label(&self) -> Option<&str> {
        None
    }

    /// Run the interactive login.
    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
        options: &'a LoginOptions,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>>;

    /// Exchange the refresh token for a fresh credential. Network call.
    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> Pin<Box<dyn Future<Output = AiResult<OAuthCredential>> + Send + 'a>>;

    /// Derive request auth from a valid credential (no I/O).
    fn to_auth(&self, credential: &OAuthCredential) -> AiResult<ModelAuth>;
}

/// Convenience alias for a boxed flow.
pub type OAuthFlowBox = Arc<dyn OAuthFlow>;

/// The `Date.now()` equivalent used for `expires` bookkeeping (epoch ms).
pub fn now_ms() -> i64 {
    crate::ai::util::now_ms() as i64
}

/// Upstream `parseAuthorizationInput`: accept a full redirect URL, a
/// `code#state` pair, a `code=...` query string, or a bare code.
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }

    let looks_like_url = value.starts_with("http://") || value.starts_with("https://");
    if looks_like_url || value.contains("code=") {
        let query = value
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or(value);
        let pairs = super::callback_server::parse_query(query);
        let code = super::callback_server::query_param(&pairs, "code").map(str::to_string);
        let state = super::callback_server::query_param(&pairs, "state").map(str::to_string);
        if looks_like_url || code.is_some() {
            return (code, state);
        }
    }

    if value.contains('#') {
        let mut parts = value.splitn(2, '#');
        let code = parts.next().map(str::to_string);
        let state = parts.next().map(str::to_string);
        return (code, state);
    }

    (Some(value.to_string()), None)
}

/// Decode a JWT payload without verifying the signature (used to read the
/// OpenAI `https://api.openai.com/auth` claim).
pub fn decode_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = crate::ai::auth::pkce::base64url_decode(parts[1]).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// Open the given URL in the user's browser: `rundll32 url.dll,FileProtocolHandler`
/// on Windows, `open` on macOS, `xdg-open` elsewhere. Best-effort — failures
/// are logged, never fatal, because the manual paste path always remains.
///
/// Upstream `open-browser.ts`: the Windows launcher must not be `cmd /c start`,
/// because cmd.exe re-parses `& | ^` inside the URL (a second parse — URL
/// injection). `rundll32` gets the URL as a single, unparsed argv element; no
/// branch goes through a shell.
pub fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    // Upstream spawn options (`open-browser.ts`): `stdio: "ignore"` — the
    // launcher must not hold the host's streams — plus `detached: true`.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `detached: true` on Windows (Node/libuv mapping): DETACHED_PROCESS
        // (0x0000_0008) + CREATE_NEW_PROCESS_GROUP (0x00000200) — the launcher
        // outlives this process and owns no console.
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // `detached: true` on Unix: the launcher leads its own process group.
        command.process_group(0);
    }
    match command.spawn() {
        // Fire-and-forget, but reap the child: on Unix a child that exits is
        // not freed by dropping the handle, it stays a zombie until waited
        // for. Node's `unref()` does not help there either — libuv reaps via
        // its SIGCHLD handler, which is the part that has no
        // drop-the-handle equivalent in std. Every OSC-8 click in the TUI
        // calls this, so the wait runs on a detached thread and the caller
        // (the click/key handler) never blocks.
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("browser-reaper".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(error) => tracing::debug!(%url, %error, "failed to open browser for OAuth login"),
    }
}

/// `PI_OAUTH_CALLBACK_HOST` override shared by the browser flows
/// (upstream `getProviderEnvValue("PI_OAUTH_CALLBACK_HOST")`).
pub fn callback_host() -> String {
    std::env::var("PI_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}
