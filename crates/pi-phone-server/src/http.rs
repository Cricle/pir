use std::collections::HashMap;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{self, AuthContext, AuthError};
use crate::metrics;
use crate::protocol::CreateDeviceRequest;
use crate::state::AppState;
use crate::ws;

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../clients/pi-phone-pwa");

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> anyhow::Result<()> {
    let sweep_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut registry = sweep_state.registry.write().await;
            registry.expire_pending(Duration::from_secs(90));
        }
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}

pub fn router(state: AppState) -> Router {
    let cors = {
        let origins: Vec<HeaderValue> = state
            .config
            .allowed_origins
            .iter()
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect();
        if origins.is_empty() {
            CorsLayer::new()
        } else {
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                ])
        }
    };
    Router::new()
        .route("/api/health", get(health))
        .route("/api/metrics", get(metrics_handler))
        .route("/api/version", get(version))
        .route("/api/me", get(me))
        .route("/api/devices", get(list_devices).post(create_device))
        .route("/api/devices/{id}", delete(delete_device))
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/messages", get(session_messages))
        .route("/api/models", get(list_models))
        .route("/api/quota", get(quota))
        .route("/api/auth/ws-ticket", post(create_ws_ticket))
        .route("/ws", get(ws::client_ws))
        .route("/agent", get(ws::agent_ws))
        .fallback(static_handler)
        .layer(cors)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Response {
    let users = state.store.count_users().unwrap_or(0);
    let devices = state.store.count_devices().unwrap_or(0);
    let sessions = state.store.count_sessions().unwrap_or(0);
    let memory = metrics::memory_snapshot();
    let registry = state.registry.read().await;
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "protocolVersion": crate::protocol::PROTOCOL_VERSION,
        "uptimeSeconds": state.started_at.elapsed().as_secs(),
        "users": users,
        "devices": devices,
        "sessions": sessions,
        "onlineDevices": registry.online_devices(),
        "onlineSessions": registry.online_sessions(),
        "clients": registry.client_count(),
        "cacheBytes": registry.cache_bytes(),
        "cachedSessions": registry.cached_sessions(),
        "subscribers": registry.subscriber_count(),
        "pendingCommands": registry.pending_commands(),
        "memory": memory,
        // Compatibility fields for the existing PWA shell.
        "hasToken": true,
        "isRunning": true,
        "serverRunning": true,
        "singleClientMode": false,
        "serverUrl": state.config.public_url.clone(),
    }))
    .into_response()
}

async fn metrics_handler(State(state): State<AppState>) -> Json<Value> {
    let memory = metrics::memory_snapshot();
    let counters = state.metrics.snapshot();
    let registry = state.registry.read().await;
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": state.started_at.elapsed().as_secs(),
        "memory": memory,
        "counters": counters,
        "registry": {
            "devices": registry.online_devices(),
            "onlineSessions": registry.online_sessions(),
            "clients": registry.client_count(),
            "subscribers": registry.subscriber_count(),
            "pendingCommands": registry.pending_commands(),
            "cacheBytes": registry.cache_bytes(),
            "cachedSessions": registry.cached_sessions(),
        }
    }))
}

async fn version() -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocolVersion": crate::protocol::PROTOCOL_VERSION,
    }))
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let user = state.store.find_user_by_id(&auth.user_id).ok().flatten();
    match user {
        Some(user) => Json(json!({ "user": user })).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "user-not-found", "user not found"),
    }
}

#[derive(Debug, Deserialize)]
struct DeviceListQuery {
    cursor: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DeviceListQuery>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let devices = match state.store.list_devices(&auth.user_id) {
        Ok(devices) => devices,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store-error",
                &error.to_string(),
            )
        }
    };
    // `cursor` is the opaque offset emitted as `nextCursor` (same contract as
    // `/api/sessions`); `offset` is accepted for direct page addressing.
    let offset = query
        .cursor
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .or(query.offset)
        .unwrap_or(0);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let total = devices.len();
    let has_more = offset.saturating_add(limit) < total;
    let next_cursor = has_more.then(|| (offset + limit).to_string());
    let devices = devices.into_iter().skip(offset).take(limit);
    let registry = state.registry.read().await;
    let mut session_counts: HashMap<&str, usize> = HashMap::new();
    for session in registry.sessions.values() {
        *session_counts
            .entry(session.device_id.as_str())
            .or_default() += 1;
    }
    let rows: Vec<Value> = devices
        .map(|device| {
            let online = registry.devices.contains_key(&device.id);
            let session_count = session_counts.get(device.id.as_str()).copied().unwrap_or(0);
            json!({
                "id": device.id,
                "name": device.name,
                "platform": device.platform,
                "createdAt": device.created_at,
                "lastSeenAt": device.last_seen_at,
                "revokedAt": device.revoked_at,
                "online": online,
                "sessionCount": session_count,
            })
        })
        .collect();
    Json(json!({ "devices": rows, "nextCursor": next_cursor, "hasMore": has_more })).into_response()
}

async fn create_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateDeviceRequest>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let existing = state.store.list_devices(&auth.user_id).unwrap_or_default();
    if existing
        .iter()
        .filter(|device| device.revoked_at.is_none())
        .count()
        >= state.config.max_devices_per_user
    {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "device-quota",
            "device quota exceeded",
        );
    }
    let device =
        match state
            .store
            .create_device(&auth.user_id, &request.name, request.platform.as_deref())
        {
            Ok(device) => device,
            Err(error) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store-error",
                    &error.to_string(),
                )
            }
        };
    let (token, _) = match state.store.create_token(
        &auth.user_id,
        "device",
        Some(&request.name),
        Some(&device.id),
    ) {
        Ok(token) => token,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store-error",
                &error.to_string(),
            )
        }
    };
    state
        .store
        .audit(Some(&auth.user_id), "device.create", Some(&device.id));
    Json(json!({ "device": device, "token": token })).into_response()
}

async fn delete_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match state.store.revoke_device(&auth.user_id, &id) {
        Ok(true) => {
            {
                let mut registry = state.registry.write().await;
                registry.unregister_device(&id, None);
                registry.remove_pending_for_device(&id);
            }
            state
                .store
                .audit(Some(&auth.user_id), "device.revoke", Some(&id));
            Json(json!({ "ok": true })).into_response()
        }
        // DELETE is idempotent: a device that is already revoked (but owned
        // by this user) is a successful no-op. Unknown and cross-user ids stay
        // 404 so ownership is never disclosed.
        Ok(false) => match state.store.find_device(&auth.user_id, &id) {
            Ok(Some(device)) if device.revoked_at.is_some() => {
                Json(json!({ "ok": true })).into_response()
            }
            _ => error_response(
                StatusCode::NOT_FOUND,
                "device-not-found",
                "device not found",
            ),
        },
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store-error",
            &error.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct SessionListQuery {
    cursor: Option<String>,
    limit: Option<usize>,
    q: Option<String>,
    #[serde(rename = "deviceId")]
    device_id: Option<String>,
}

async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SessionListQuery>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let offset = query
        .cursor
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let mut sessions = match state.store.list_sessions(
        &auth.user_id,
        offset,
        limit.saturating_add(1),
        query.q.as_deref(),
        query.device_id.as_deref(),
    ) {
        Ok(sessions) => sessions,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store-error",
                &error.to_string(),
            )
        }
    };
    let has_more = sessions.len() > limit;
    sessions.truncate(limit);
    let next_cursor = has_more.then(|| (offset + sessions.len()).to_string());
    Json(json!({
        "sessions": sessions,
        "nextCursor": next_cursor,
        "hasMore": has_more,
    }))
    .into_response()
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match state.store.get_session(&auth.user_id, &id) {
        Ok(Some(session)) => Json(json!({ "session": session })).into_response(),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "session-not-found",
            "session not found",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store-error",
            &error.to_string(),
        ),
    }
}

async fn delete_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match state.store.soft_delete_session(&auth.user_id, &id) {
        Ok(true) => {
            {
                let mut registry = state.registry.write().await;
                if registry
                    .sessions
                    .get(&id)
                    .is_some_and(|session| session.user_id == auth.user_id)
                {
                    registry.remove_session_id(&id);
                }
            }
            state
                .store
                .audit(Some(&auth.user_id), "session.delete", Some(&id));
            Json(json!({ "ok": true })).into_response()
        }
        Ok(false) => error_response(
            StatusCode::NOT_FOUND,
            "session-not-found",
            "session not found",
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store-error",
            &error.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    before: Option<usize>,
    limit: Option<usize>,
}

async fn session_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let payload = match ws::request_agent(
        &state,
        &auth.user_id,
        &id,
        json!({"kind":"rpc","command":{"type":"get_messages"}}),
        Duration::from_secs(30),
    )
    .await
    {
        Ok(payload) => payload,
        Err(error) => {
            let status = if error == "agent-offline" {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_GATEWAY
            };
            return error_response(status, &error, &error);
        }
    };
    let data = payload
        .get("payload")
        .and_then(|payload| payload.get("data"))
        .cloned()
        .unwrap_or(Value::Null);
    let messages = data
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = messages.len();
    let limit = query.limit.unwrap_or(60).clamp(1, 200);
    let before = query.before.unwrap_or(total).min(total);
    let start = before.saturating_sub(limit);
    let page = messages[start..before].to_vec();
    Json(json!({
        "messages": page,
        "hasMore": start > 0,
        "totalMessageCount": total,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct ModelQuery {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

async fn first_online_session(state: &AppState, user_id: &str) -> Option<String> {
    let registry = state.registry.read().await;
    registry
        .sessions
        .values()
        .find(|session| session.user_id == user_id && session.online)
        .map(|session| session.id.clone())
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelQuery>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let Some(session_id) = query
        .session_id
        .or(first_online_session(&state, &auth.user_id).await)
    else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent-offline",
            "no online session",
        );
    };
    match ws::request_agent(
        &state,
        &auth.user_id,
        &session_id,
        json!({"kind":"rpc","command":{"type":"get_available_models"}}),
        Duration::from_secs(30),
    )
    .await
    {
        Ok(payload) => {
            let data = payload
                .get("payload")
                .and_then(|payload| payload.get("data"))
                .cloned()
                .unwrap_or(Value::Null);
            Json(json!({ "models": data.get("models").cloned().unwrap_or(json!([])) }))
                .into_response()
        }
        Err(error) => error_response(StatusCode::BAD_GATEWAY, &error, &error),
    }
}

#[derive(Debug, Deserialize)]
struct QuotaQuery {
    provider: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[allow(dead_code)]
    force: Option<String>,
}

async fn quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<QuotaQuery>,
) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let Some(session_id) = query
        .session_id
        .or(first_online_session(&state, &auth.user_id).await)
    else {
        return Json(json!({ "visible": false })).into_response();
    };
    let payload = ws::request_agent(
        &state,
        &auth.user_id,
        &session_id,
        json!({
            "kind": "rpc",
            "command": {
                "type": "get_quota",
                "provider": query.provider,
                "modelId": query.model_id,
            }
        }),
        Duration::from_secs(30),
    )
    .await;
    match payload {
        Ok(payload) => Json(
            payload
                .get("payload")
                .and_then(|payload| payload.get("data"))
                .cloned()
                .unwrap_or_else(|| json!({ "visible": false })),
        )
        .into_response(),
        // Quota is optional; a provider/agent failure must not break the chat.
        Err(_) => Json(json!({ "visible": false })).into_response(),
    }
}

async fn create_ws_ticket(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let auth = match require_user(&state, &headers) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let ticket = state.tickets.issue(&auth.user_id);
    Json(json!({ "ticket": ticket, "expiresInSeconds": 30 })).into_response()
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // Never let an unknown API/WS path fall back to the SPA shell; clients
    // need a real 404 rather than HTML with status 200.
    if path.starts_with("api/") || path == "ws" || path == "agent" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    let file = ASSETS
        .get_file(path)
        .or_else(|| ASSETS.get_file("index.html"));
    let Some(file) = file else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(file.path())
        .first_or_octet_stream()
        .to_string();
    let mut response = axum::body::Body::from(Bytes::from_static(file.contents())).into_response();
    if let Ok(content_type) = HeaderValue::from_str(&mime) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, content_type);
    }
    let path = file.path().to_string_lossy();
    let cache = if path.ends_with(".html")
        || path.ends_with("sw.js")
        || path.ends_with(".webmanifest")
        || path.ends_with(".js")
        || path.ends_with(".mjs")
        || path.ends_with(".css")
    {
        "no-cache"
    } else {
        "public, max-age=300"
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static(cache),
    );
    response
}

#[allow(clippy::result_large_err)]
fn require_user(state: &AppState, headers: &HeaderMap) -> Result<AuthContext, Response> {
    auth::authenticate_bearer(&state.store, headers, "user").map_err(auth_error_response)
}

fn auth_error_response(error: AuthError) -> Response {
    let status = match error {
        AuthError::Missing | AuthError::Invalid => StatusCode::UNAUTHORIZED,
        AuthError::Revoked | AuthError::Disabled => StatusCode::FORBIDDEN,
        AuthError::WrongKind => StatusCode::FORBIDDEN,
        AuthError::Origin => StatusCode::FORBIDDEN,
    };
    error_response(status, "unauthorized", &error.to_string())
}

pub fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}
