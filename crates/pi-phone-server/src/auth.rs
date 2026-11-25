use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::store::{Store, TokenRow};

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: String,
    pub username: String,
    pub token_id: String,
    pub token_kind: String,
    pub device_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing bearer token")]
    Missing,
    #[error("invalid bearer token")]
    Invalid,
    #[error("token revoked")]
    Revoked,
    #[error("user disabled")]
    Disabled,
    #[error("wrong token kind")]
    WrongKind,
    #[error("cross-origin request rejected")]
    Origin,
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    if let Err(error) = getrandom::fill(&mut bytes) {
        // getrandom failure is fatal for a server that issues credentials.
        panic!("secure random source unavailable: {error}");
    }
    format!("pp_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn hash_token(pepper: &str, token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.update(pepper.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn constant_time_eq(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    value.strip_prefix("Bearer ").map(str::trim)
}

pub fn authenticate_bearer(
    store: &Store,
    headers: &HeaderMap,
    expected_kind: &str,
) -> Result<AuthContext, AuthError> {
    let token = bearer_token(headers).ok_or(AuthError::Missing)?;
    authenticate_token(store, token, expected_kind)
}

pub fn authenticate_token(
    store: &Store,
    token: &str,
    expected_kind: &str,
) -> Result<AuthContext, AuthError> {
    let hash = hash_token(&store.pepper, token);
    let row = store
        .find_token_by_hash(&hash)
        .map_err(|_| AuthError::Invalid)?
        .ok_or(AuthError::Invalid)?;
    validate_token(store, row, expected_kind)
}

fn validate_token(
    store: &Store,
    row: TokenRow,
    expected_kind: &str,
) -> Result<AuthContext, AuthError> {
    if row.revoked_at.is_some() {
        return Err(AuthError::Revoked);
    }
    if row.kind != expected_kind {
        return Err(AuthError::WrongKind);
    }
    let user = store
        .find_user_by_id(&row.user_id)
        .map_err(|_| AuthError::Invalid)?
        .ok_or(AuthError::Invalid)?;
    if user.disabled_at.is_some() {
        return Err(AuthError::Disabled);
    }
    let _ = store.touch_token(&row.id);
    Ok(AuthContext {
        user_id: user.id,
        username: user.username,
        token_id: row.id,
        token_kind: row.kind,
        device_id: row.device_id,
    })
}

/// Browser WebSocket clients cannot set `Authorization`. They send
/// `Sec-WebSocket-Protocol: pi-phone.v1, bearer.<base64url-token>` and the
/// server selects `pi-phone.v1`.
pub fn bearer_from_subprotocol(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?;
    for part in value.split(',') {
        let part = part.trim();
        if let Some(encoded) = part.strip_prefix("bearer.") {
            let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

pub fn origin_allowed(headers: &HeaderMap, allowed_origins: &[String]) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        // Non-browser agents do not send Origin.
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if origin.eq_ignore_ascii_case("null") {
        return false;
    }
    if allowed_origins.iter().any(|allowed| allowed == origin) {
        return true;
    }
    let Some(origin_host) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next())
    else {
        return false;
    };
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(host)
}

#[derive(Debug, Clone)]
pub struct TicketIdentity {
    pub user_id: String,
    pub token_id: String,
}

#[derive(Debug, Clone)]
struct Ticket {
    user_id: String,
    token_id: String,
    expires_at: Instant,
}

const MAX_LIVE_TICKETS: usize = 4_096;
const MAX_TICKETS_PER_USER: usize = 4;

#[derive(Default)]
struct TicketState {
    tickets: HashMap<String, Ticket>,
    per_user: HashMap<String, usize>,
}

/// One-use WebSocket tickets for clients that cannot send a bearer
/// subprotocol. Tickets are intentionally short-lived and memory-only.
#[derive(Default)]
pub struct TicketStore {
    inner: Mutex<TicketState>,
}

impl TicketStore {
    /// Issue one single-use ticket; `None` when a live-ticket quota is full
    /// (the caller answers 429 — the only failure this can have).
    pub fn issue(&self, user_id: &str, token_id: &str) -> Option<String> {
        let ticket = generate_token();
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        let mut expired = Vec::new();
        state.tickets.retain(|key, entry| {
            let live = entry.expires_at > now;
            if !live {
                expired.push((key.clone(), entry.user_id.clone()));
            }
            live
        });
        for (_, expired_user) in expired {
            decrement_ticket_count(&mut state.per_user, &expired_user);
        }
        if state.tickets.len() >= MAX_LIVE_TICKETS
            || state.per_user.get(user_id).copied().unwrap_or(0) >= MAX_TICKETS_PER_USER
        {
            return None;
        }
        state.tickets.insert(
            ticket.clone(),
            Ticket {
                user_id: user_id.to_owned(),
                token_id: token_id.to_owned(),
                expires_at: now + Duration::from_secs(30),
            },
        );
        *state.per_user.entry(user_id.to_owned()).or_default() += 1;
        Some(ticket)
    }

    pub fn redeem(&self, ticket: &str) -> Option<TicketIdentity> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = state.tickets.remove(ticket)?;
        decrement_ticket_count(&mut state.per_user, &entry.user_id);
        if entry.expires_at <= Instant::now() {
            return None;
        }
        Some(TicketIdentity {
            user_id: entry.user_id,
            token_id: entry.token_id,
        })
    }
}

fn decrement_ticket_count(counts: &mut HashMap<String, usize>, user_id: &str) {
    if let Some(count) = counts.get_mut(user_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(user_id);
        }
    }
}
