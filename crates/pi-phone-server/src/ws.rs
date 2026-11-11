use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::auth::{self, AuthContext};
use crate::metrics::Metrics;
use crate::protocol::{
    json_line, AgentEnvelope, AgentHello, ClientEnvelope, ModelDescriptor, SessionDescriptor,
};
use crate::registry::{ClientId, PendingReply};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub ticket: Option<String>,
}

pub async fn agent_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !auth::origin_allowed(&headers, &state.config.allowed_origins) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let auth = match auth::authenticate_bearer(&state.store, &headers, "device") {
        Ok(auth) => auth,
        Err(error) => {
            return crate::http::error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                &error.to_string(),
            )
        }
    };
    if state.registry.read().await.devices.len() >= state.config.max_connections {
        return crate::http::error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "connection-limit",
            "server connection limit reached",
        );
    }
    let max = state.config.max_ws_message_bytes;
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| agent_loop(state, auth, socket))
}

async fn agent_loop(state: AppState, auth: AuthContext, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Arc<str>>(state.config.max_agent_queue);
    Metrics::incr(&state.metrics.ws_connections);

    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if sender
                .send(Message::Text(line.as_ref().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let first = tokio::time::timeout(Duration::from_secs(10), receiver.next()).await;
    let hello = match first {
        Ok(Some(Ok(Message::Text(text)))) => {
            let value: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    let _ = tx
                        .send(json_line(json!({"type":"hello-error","error":{"code":"invalid-json","message":error.to_string()}})))
                        .await;
                    drop(tx);
                    let _ = writer.await;
                    return;
                }
            };
            if value.get("type").and_then(Value::as_str) != Some("hello") {
                let _ = tx
                    .send(json_line(json!({"type":"hello-error","error":{"code":"expected-hello","message":"first frame must be hello"}})))
                    .await;
                drop(tx);
                let _ = writer.await;
                return;
            }
            match serde_json::from_value::<AgentHello>(value) {
                Ok(hello) => hello,
                Err(error) => {
                    let _ = tx
                        .send(json_line(json!({"type":"hello-error","error":{"code":"invalid-hello","message":error.to_string()}})))
                        .await;
                    drop(tx);
                    let _ = writer.await;
                    return;
                }
            }
        }
        _ => {
            drop(tx);
            let _ = writer.await;
            return;
        }
    };

    if hello.version != crate::protocol::PROTOCOL_VERSION {
        let _ = tx
            .send(json_line(json!({"type":"hello-error","error":{"code":"version-mismatch","message":"unsupported protocol version"}})))
            .await;
        drop(tx);
        let _ = writer.await;
        return;
    }
    if hello.sessions.len() > state.config.max_sessions_per_user {
        let _ = tx
            .send(json_line(json!({"type":"hello-error","error":{"code":"session-quota","message":"too many sessions in hello"}})))
            .await;
        drop(tx);
        let _ = writer.await;
        return;
    }
    if state
        .store
        .find_device_by_id(&hello.device_id)
        .ok()
        .flatten()
        .is_none()
    {
        let active_devices = state
            .store
            .list_devices(&auth.user_id)
            .unwrap_or_default()
            .iter()
            .filter(|device| device.revoked_at.is_none())
            .count();
        if active_devices >= state.config.max_devices_per_user {
            let _ = tx
                .send(json_line(json!({"type":"hello-error","error":{"code":"device-quota","message":"device quota exceeded"}})))
                .await;
            drop(tx);
            let _ = writer.await;
            return;
        }
    }
    if let Some(bound_device) = auth.device_id.as_deref() {
        if !bound_device.is_empty() && bound_device != hello.device_id {
            let _ = tx
                .send(json_line(json!({"type":"hello-error","error":{"code":"device-mismatch","message":"token is bound to another device"}})))
                .await;
            drop(tx);
            let _ = writer.await;
            return;
        }
    }

    let device_name = hello
        .device_name
        .clone()
        .unwrap_or_else(|| hello.device_id.clone());
    if let Err(error) = state.store.ensure_device(
        &auth.user_id,
        &hello.device_id,
        &device_name,
        hello.platform.as_deref(),
    ) {
        let _ = tx
            .send(json_line(json!({"type":"hello-error","error":{"code":"device-registration-failed","message":error.to_string()}})))
            .await;
        drop(tx);
        let _ = writer.await;
        return;
    }
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    let (session_ids, connection_id) = {
        let mut registry = state.registry.write().await;
        registry.register_device(&auth.user_id, &hello, tx.clone(), Some(close_tx))
    };
    if let Err(error) =
        state
            .store
            .upsert_sessions_batch(&auth.user_id, &hello.device_id, &hello.sessions, true)
    {
        tracing::warn!("batch session upsert failed: {error}");
        for descriptor in &hello.sessions {
            if let Err(error) =
                state
                    .store
                    .upsert_session(&auth.user_id, &hello.device_id, descriptor, true)
            {
                tracing::warn!("session upsert failed for {}: {error}", descriptor.id);
            }
        }
    }
    let _ = state.store.touch_device(&hello.device_id);
    state
        .store
        .audit(Some(&auth.user_id), "agent.connect", Some(&hello.device_id));

    let hello_ok = json!({
        "type": "hello-ok",
        "protocolVersion": crate::protocol::PROTOCOL_VERSION,
        "deviceId": hello.device_id,
        "sessionIds": session_ids,
        "capabilities": crate::protocol::SERVER_CAPABILITIES,
    });
    if tx.send(json_line(hello_ok)).await.is_err() {
        drop(tx);
        let _ = writer.await;
        return;
    }
    broadcast_catalog(&state, &auth.user_id).await;

    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            message = receiver.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        last_seen = Instant::now();
                        Metrics::incr(&state.metrics.agent_frames);
                        handle_agent_text(
                            &state,
                            &auth,
                            &hello.device_id,
                            connection_id,
                            &text,
                        )
                        .await;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                        last_seen = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_))) => {}
                    Some(Err(_)) => break,
                }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(state.config.idle_timeout_secs) {
                    break;
                }
                if tx.try_send(json_line(json!({"type":"ping"}))).is_err() {
                    break;
                }
            }
            _ = &mut close_rx => break,
        }
    }

    let cleanup_current = {
        let mut registry = state.registry.write().await;
        let current = registry
            .devices
            .get(&hello.device_id)
            .is_some_and(|device| device.connection_id == connection_id);
        registry.unregister_device(&hello.device_id, Some(connection_id));
        if current {
            registry.remove_pending_for_device(&hello.device_id);
        }
        current
    };
    if cleanup_current {
        let _ = state.store.mark_device_sessions_offline(&hello.device_id);
        state.store.audit(
            Some(&auth.user_id),
            "agent.disconnect",
            Some(&hello.device_id),
        );
        broadcast_catalog(&state, &auth.user_id).await;
    }
    drop(tx);
    let _ = writer.await;
}

async fn handle_agent_text(
    state: &AppState,
    auth: &AuthContext,
    device_id: &str,
    connection_id: u64,
    text: &str,
) {
    let Ok(envelope) = serde_json::from_str::<AgentEnvelope>(text) else {
        tracing::debug!(device_id, bytes = text.len(), "agent envelope rejected");
        return;
    };
    {
        let registry = state.registry.read().await;
        if !registry
            .devices
            .get(device_id)
            .is_some_and(|device| device.connection_id == connection_id)
        {
            return;
        }
    }
    match envelope.r#type.as_str() {
        "frame" => {
            Metrics::incr(&state.metrics.event_frames);
            let Some(local_id) = envelope.session_id else {
                return;
            };
            let Some(payload) = envelope.payload else {
                return;
            };
            // Controlled custom-event lane: namespaced, size-bounded, and
            // relayed only for an existing owned session.
            if payload
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("phone.custom."))
                && serde_json::to_vec(&payload)
                    .map(|bytes| bytes.len() > 64 * 1024)
                    .unwrap_or(true)
            {
                return;
            }
            let catalog_id = format!("{device_id}:{local_id}");
            let custom_event = payload
                .get("type")
                .and_then(Value::as_str)
                .filter(|name| name.starts_with("phone.custom."));
            let (live_connection, owns_session, custom_allowed) = {
                let registry = state.registry.read().await;
                let device = registry.devices.get(device_id);
                let live = device.is_some_and(|device| device.connection_id == connection_id);
                let custom_allowed = custom_event.is_none_or(|name| {
                    device.is_some_and(|device| {
                        device.custom_events.contains(name)
                            || device.custom_events.contains("phone.custom.*")
                    })
                });
                let owned = registry
                    .sessions
                    .get(&catalog_id)
                    .map(|session| {
                        session.device_id == device_id && session.user_id == auth.user_id
                    })
                    .unwrap_or(false);
                (live, owned, custom_allowed)
            };
            if !live_connection || !owns_session || !custom_allowed {
                tracing::debug!(
                    device_id,
                    local_id,
                    live = live_connection,
                    owned = owns_session,
                    custom_allowed,
                    "agent frame dropped"
                );
                return;
            }
            // The agent's status frame carries the live label/model/thinking/
            // streaming state. Keep the catalog row fresh without requiring a
            // separate metadata message.
            if let Some(descriptor) = descriptor_from_status(&payload, &local_id) {
                let changed = {
                    let mut registry = state.registry.write().await;
                    registry
                        .upsert_session(&auth.user_id, device_id, &descriptor, true)
                        .1
                };
                if changed {
                    let _ = state
                        .store
                        .upsert_session(&auth.user_id, device_id, &descriptor, true);
                    broadcast_catalog(state, &auth.user_id).await;
                }
            }
            // Responses are routed to the originating phone only, never
            // broadcast to every subscriber of the session.
            if let Some(forward_id) = response_forward_id(&payload) {
                let pending = {
                    let mut registry = state.registry.write().await;
                    registry.take_pending_for(&forward_id, &auth.user_id, device_id, &catalog_id)
                };
                if let Some(pending) = pending {
                    let mut corrected = payload;
                    match pending.reply {
                        crate::registry::PendingReply::Client {
                            client_id,
                            original_id,
                        } => {
                            if let Some(original) = original_id {
                                if let Some(object) =
                                    corrected.get_mut("payload").and_then(Value::as_object_mut)
                                {
                                    object.insert("id".to_owned(), Value::String(original));
                                }
                            }
                            let line = json_line(corrected);
                            let registry = state.registry.read().await;
                            if let Some(client) = registry.clients.get(&client_id) {
                                let _ = client.tx.try_send(line);
                            }
                        }
                        crate::registry::PendingReply::Http { tx } => {
                            let _ = tx.send(corrected);
                        }
                    }
                    return;
                }
            }
            {
                let line = crate::protocol::json_line_ref(&payload);
                let mut registry = state.registry.write().await;
                registry.cache_frame(&catalog_id, &payload, &line);
                let deliveries = registry.broadcast(&catalog_id, &line);
                Metrics::incr(&state.metrics.broadcast_frames);
                state
                    .metrics
                    .broadcast_deliveries
                    .fetch_add(deliveries as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        "session-upsert" => {
            if let Some(descriptor) = envelope.session {
                let changed = {
                    let mut registry = state.registry.write().await;
                    registry
                        .upsert_session(&auth.user_id, device_id, &descriptor, true)
                        .1
                };
                tracing::debug!(
                    device_id,
                    session_id = descriptor.id.as_str(),
                    kind = descriptor.kind.as_deref().unwrap_or(""),
                    changed,
                    "agent session upsert"
                );
                if changed {
                    let _ = state
                        .store
                        .upsert_session(&auth.user_id, device_id, &descriptor, true);
                    broadcast_catalog(state, &auth.user_id).await;
                }
            }
        }
        "session-remove" => {
            if let Some(session_id) = envelope.session_id {
                let catalog_id = format!("{device_id}:{session_id}");
                let _ = state.store.soft_delete_session(&auth.user_id, &catalog_id);
                {
                    let mut registry = state.registry.write().await;
                    registry.remove_session(device_id, &session_id);
                }
                broadcast_catalog(state, &auth.user_id).await;
            }
        }
        "pong" | "ping" => {}
        _ => {}
    }
}

fn response_forward_id(payload: &Value) -> Option<String> {
    if payload.get("channel").and_then(Value::as_str) != Some("rpc") {
        return None;
    }
    let inner = payload.get("payload")?;
    if inner.get("type").and_then(Value::as_str) != Some("response") {
        return None;
    }
    inner.get("id").and_then(Value::as_str).map(str::to_owned)
}

pub async fn client_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !auth::origin_allowed(&headers, &state.config.allowed_origins) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let user_id = if let Some(ticket) = query.ticket.as_deref() {
        match state.tickets.redeem(ticket) {
            Some(user_id) => user_id,
            None => {
                return crate::http::error_response(
                    StatusCode::UNAUTHORIZED,
                    "invalid-ticket",
                    "invalid or expired WebSocket ticket",
                )
            }
        }
    } else if let Some(token) = auth::bearer_from_subprotocol(&headers)
        .or_else(|| auth::bearer_token(&headers).map(str::to_owned))
    {
        match auth::authenticate_token(&state.store, &token, "user") {
            Ok(auth) => auth.user_id,
            Err(error) => {
                return crate::http::error_response(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    &error.to_string(),
                )
            }
        }
    } else {
        return crate::http::error_response(
            StatusCode::UNAUTHORIZED,
            "missing-token",
            "user token is required",
        );
    };

    if state.registry.read().await.client_count() >= state.config.max_connections {
        return crate::http::error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "connection-limit",
            "server connection limit reached",
        );
    }
    let max = state.config.max_ws_message_bytes;
    ws.max_message_size(max)
        .max_frame_size(max)
        .protocols(["pi-phone.v1"])
        .on_upgrade(move |socket| client_loop(state, user_id, socket))
}

async fn client_loop(state: AppState, user_id: String, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Arc<str>>(state.config.max_client_queue);
    Metrics::incr(&state.metrics.ws_connections);
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if sender
                .send(Message::Text(line.as_ref().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let client_id: ClientId = {
        let mut registry = state.registry.write().await;
        registry.alloc_client(&user_id, tx.clone())
    };

    let _ = tx.try_send(json_line(build_status(&state, &user_id).await));
    let _ = tx.try_send(json_line(
        build_catalog(&state, &user_id, None, 0, 50, None, None).await,
    ));

    let mut active_session: Option<String> = None;
    let mut ping = tokio::time::interval(Duration::from_secs(60));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            message = receiver.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        Metrics::incr(&state.metrics.client_frames);
                        handle_client_text(
                            &state,
                            &user_id,
                            client_id,
                            &tx,
                            &mut active_session,
                            &text,
                        )
                        .await;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_))) => {}
                    Some(Err(_)) => break,
                }
            }
            _ = ping.tick() => {
                if tx.try_send(json_line(json!({"channel":"server","event":"ping","data":{}}))).is_err() {
                    break;
                }
            }
        }
    }

    {
        let mut registry = state.registry.write().await;
        if let Some(session_id) = &active_session {
            registry.unsubscribe(client_id, session_id);
        }
        registry.remove_pending_for_client(client_id);
        registry.remove_client(client_id);
    }
    drop(tx);
    let _ = writer.await;
}

async fn handle_client_text(
    state: &AppState,
    user_id: &str,
    client_id: ClientId,
    tx: &mpsc::Sender<Arc<str>>,
    active_session: &mut Option<String>,
    text: &str,
) {
    let Ok(envelope) = serde_json::from_str::<ClientEnvelope>(text) else {
        let _ = tx.try_send(server_error("invalid-json", "invalid JSON"));
        return;
    };
    match envelope.kind.as_str() {
        "sessions/list" => {
            let offset = envelope
                .cursor
                .as_deref()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let limit = envelope.limit.unwrap_or(50).clamp(1, 200);
            let catalog = build_catalog(
                state,
                user_id,
                active_session.as_deref(),
                offset,
                limit,
                envelope.q.as_deref(),
                envelope.device_id.as_deref(),
            )
            .await;
            let _ = tx.try_send(json_line(catalog));
        }
        "session-select" => {
            let Some(session_id) = envelope.session_id else {
                let _ = tx.try_send(server_error("missing-session", "sessionId is required"));
                return;
            };
            let owned = state
                .store
                .get_session(user_id, &session_id)
                .ok()
                .flatten()
                .is_some();
            if !owned {
                tracing::debug!("session-select rejected (not owned): {session_id}");
                let _ = tx.try_send(server_error("session-not-found", "session not found"));
                return;
            }
            {
                let mut registry = state.registry.write().await;
                if let Some(old) = active_session.take() {
                    registry.unsubscribe(client_id, &old);
                }
                let already_subscribed = registry
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.subscribers.contains_key(&client_id));
                let subscriber_count = registry
                    .sessions
                    .get(&session_id)
                    .map(|session| session.subscribers.len())
                    .unwrap_or(0);
                if !already_subscribed && subscriber_count >= state.config.max_clients_per_session {
                    let _ = tx.try_send(server_error(
                        "session-client-limit",
                        "too many phones subscribed to this session",
                    ));
                    return;
                }
                if registry.subscribe(client_id, &session_id).is_none() {
                    tracing::debug!("session-select rejected (not online): {session_id}");
                    let _ = tx.try_send(server_error("session-not-found", "session is not online"));
                    return;
                }
                *active_session = Some(session_id.clone());
                tracing::debug!("session-select accepted: {session_id}");
            }
            let cached = {
                let registry = state.registry.read().await;
                registry.sessions.get(&session_id).map(|session| {
                    (
                        session.cached_status.clone(),
                        session.local_id.clone(),
                        session.device_id.clone(),
                    )
                })
            };
            // Acknowledge selection before cached frames so clients can update
            // their active-session state even when they are not consuming the
            // snapshot yet (and tests/clients can wait for this frame first).
            let _ = tx.try_send(json_line(json!({
                "channel": "sessions",
                "event": "selected",
                "data": { "sessionId": session_id }
            })));
            if let Some((status, local_id, device_id)) = cached {
                if let Some(status) = status {
                    let _ = tx.try_send(status);
                }
                // Never serve transcript content from server memory: ask the
                // owning agent for a fresh snapshot instead.
                let registry = state.registry.read().await;
                if let Some(device) = registry.devices.get(&device_id) {
                    let _ = device.tx.try_send(json_line(json!({
                        "type":"resync","sessionId":local_id,"reason":"client-joined"
                    })));
                }
            }
        }
        "refresh"
        | "rpc"
        | "local-command"
        | "load-earlier-messages"
        | "session-parent-new"
        | "session-spawn" => {
            let Some(session_id) = active_session.clone() else {
                tracing::debug!("command rejected: no active session");
                let _ = tx.try_send(server_error("no-session", "select a session first"));
                return;
            };
            forward_command(state, user_id, client_id, &session_id, text).await;
        }
        "ping" => {
            let _ = tx.try_send(json_line(
                json!({"channel":"server","event":"pong","data":{}}),
            ));
        }
        other => {
            let _ = tx.try_send(server_error(
                "unknown-message",
                &format!("unknown message kind: {other}"),
            ));
        }
    }
}

async fn forward_command(
    state: &AppState,
    user_id: &str,
    client_id: ClientId,
    session_id: &str,
    text: &str,
) {
    let (device_id, local_id, online) = {
        let registry = state.registry.read().await;
        let Some(session) = registry.sessions.get(session_id) else {
            return;
        };
        if session.user_id != user_id {
            return;
        }
        (
            session.device_id.clone(),
            session.local_id.clone(),
            session.online,
        )
    };
    if !online {
        let registry = state.registry.read().await;
        if let Some(client) = registry.clients.get(&client_id) {
            let _ = client
                .tx
                .try_send(server_error("agent-offline", "device is offline"));
        }
        return;
    }

    let mut value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return,
    };
    let mut original_id = None;
    let forward_id = {
        let mut registry = state.registry.write().await;
        registry.next_forward_id()
    };

    if value.get("kind").and_then(Value::as_str) == Some("rpc") {
        original_id = apply_forward_id(&mut value, &forward_id);
    } else if value.get("kind").and_then(Value::as_str) == Some("load-earlier-messages") {
        if let Some(existing) = value
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            original_id = Some(existing);
        }
        value["requestId"] = Value::String(forward_id.clone());
    }

    let expects_response = !matches!(
        value.get("kind").and_then(Value::as_str),
        Some("refresh" | "load-earlier-messages" | "session-parent-new" | "session-spawn")
    );
    if expects_response {
        let mut registry = state.registry.write().await;
        registry.add_pending(
            forward_id.clone(),
            crate::registry::PendingReply::Client {
                client_id,
                original_id,
            },
            user_id,
            &device_id,
            session_id,
        );
    }
    Metrics::incr(&state.metrics.commands);
    let line: Arc<str> = Arc::from(value.to_string());
    let device_tx = {
        let registry = state.registry.read().await;
        registry
            .devices
            .get(&device_id)
            .map(|device| device.tx.clone())
    };
    if let Some(device_tx) = device_tx {
        if device_tx
            .try_send(json_line(json!({
                "type":"command","sessionId":local_id,"forwardId":forward_id,"line":line.as_ref()
            })))
            .is_err()
        {
            if expects_response {
                let mut registry = state.registry.write().await;
                registry.take_pending(&forward_id);
            }
            let registry = state.registry.read().await;
            if let Some(client) = registry.clients.get(&client_id) {
                let _ = client
                    .tx
                    .try_send(server_error("agent-offline", "device is offline"));
            }
        }
    } else {
        if expects_response {
            let mut registry = state.registry.write().await;
            registry.take_pending(&forward_id);
        }
        let registry = state.registry.read().await;
        if let Some(client) = registry.clients.get(&client_id) {
            let _ = client
                .tx
                .try_send(server_error("agent-offline", "device is offline"));
        }
    }
}

/// Stamp the relay's request/response correlation id onto one client command
/// and return the phone's original `id` (restored on the response).
///
/// Ordinary rpc commands use `command.id` only for correlation, so it is
/// replaced with `forward_id`. `extension_ui_response` is different: its `id`
/// addresses the pending dialog inside the agent session and must reach the
/// agent unchanged (the phone bridge and the extension runtime both match on
/// it). The correlation id travels as the envelope's `requestId` instead; the
/// phone bridge reads it back when emitting the response.
fn apply_forward_id(value: &mut Value, forward_id: &str) -> Option<String> {
    let original_id;
    let keep_dialog_id;
    {
        let command = value.get_mut("command").and_then(Value::as_object_mut)?;
        let command_type = command
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned);
        original_id = command.get("id").and_then(Value::as_str).map(str::to_owned);
        keep_dialog_id = command_type.as_deref() == Some("extension_ui_response");
        if !keep_dialog_id {
            command.insert("id".to_owned(), Value::String(forward_id.to_owned()));
        }
    }
    if keep_dialog_id {
        if let Some(object) = value.as_object_mut() {
            object.insert("requestId".to_owned(), Value::String(forward_id.to_owned()));
        }
    }
    original_id
}

async fn build_status(state: &AppState, user_id: &str) -> Value {
    let user = state.store.find_user_by_id(user_id).ok().flatten();
    let devices = state.store.list_devices(user_id).unwrap_or_default();
    let registry = state.registry.read().await;
    json!({
        "channel": "server",
        "event": "status",
        "data": {
            "serverVersion": env!("CARGO_PKG_VERSION"),
            "protocolVersion": crate::protocol::PROTOCOL_VERSION,
            "capabilities": crate::protocol::SERVER_CAPABILITIES,
            "user": user,
            "deviceCount": devices.len(),
            "onlineDevices": registry.online_devices(),
            "onlineSessions": registry.online_sessions(),
            "connectedClients": registry.client_count(),
            // Compatibility fields for the existing PWA header/status card.
            "hasToken": true,
            "isRunning": true,
            "serverRunning": true,
            "singleClientMode": false,
            "serverUrl": state.config.public_url.clone(),
            // Relay-owned status has no agent queue/theme: the boot frame
            // reports an empty queue so the PWA does not clear chips with an
            // undefined value. Agent `server/status` frames that follow a
            // session selection carry the authoritative `pendingMessages`.
            // `theme` is intentionally absent (optional for the relay).
            "pendingMessages": [],
            "host": state.config.listen.ip().to_string(),
            "port": state.config.listen.port(),
            "cwd": Value::Null,
            "model": Value::Null,
            "isStreaming": false,
            "controlOwner": "server",
        }
    })
}

/// Send one RPC command to the agent that owns `session_id` and await its
/// response. Used by the HTTP history/model endpoints; the PWA still uses the
/// WebSocket path for interactive commands.
pub async fn request_agent(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    mut value: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let (device_id, local_id, online) = {
        let registry = state.registry.read().await;
        let session = registry
            .sessions
            .get(session_id)
            .ok_or_else(|| "session-not-found".to_owned())?;
        if session.user_id != user_id {
            return Err("session-not-found".to_owned());
        }
        (
            session.device_id.clone(),
            session.local_id.clone(),
            session.online,
        )
    };
    if !online {
        return Err("agent-offline".to_owned());
    }

    let forward_id = {
        let mut registry = state.registry.write().await;
        let forward_id = registry.next_forward_id();
        if value.get("kind").and_then(Value::as_str) == Some("rpc") {
            if let Some(command) = value.get_mut("command").and_then(Value::as_object_mut) {
                command.insert("id".to_owned(), Value::String(forward_id.clone()));
            }
        }
        let Some(device_tx) = registry
            .devices
            .get(&device_id)
            .map(|device| device.tx.clone())
        else {
            return Err("agent-offline".to_owned());
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        registry.add_pending(
            forward_id.clone(),
            PendingReply::Http { tx },
            user_id,
            &device_id,
            session_id,
        );
        let envelope = json!({
            "type": "command",
            "sessionId": local_id,
            "forwardId": forward_id,
            "line": value.to_string(),
        });
        if device_tx.try_send(json_line(envelope)).is_err() {
            registry.take_pending(&forward_id);
            return Err("agent-offline".to_owned());
        }
        (forward_id, rx)
    };

    match tokio::time::timeout(timeout, forward_id.1).await {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(_)) => Err("agent-disconnected".to_owned()),
        Err(_) => {
            let mut registry = state.registry.write().await;
            registry.take_pending(&forward_id.0);
            Err("agent-timeout".to_owned())
        }
    }
}

async fn broadcast_catalog(state: &AppState, user_id: &str) {
    let catalog = json_line(build_catalog(state, user_id, None, 0, 50, None, None).await);
    Metrics::incr(&state.metrics.catalog_broadcasts);
    let registry = state.registry.read().await;
    for client in registry.clients.values() {
        if client.user_id == user_id {
            let _ = client.tx.try_send(Arc::clone(&catalog));
        }
    }
}

async fn build_catalog(
    state: &AppState,
    user_id: &str,
    active: Option<&str>,
    offset: usize,
    limit: usize,
    query: Option<&str>,
    device_id: Option<&str>,
) -> Value {
    let mut sessions = state
        .store
        .list_sessions(user_id, offset, limit.saturating_add(1), query, device_id)
        .unwrap_or_default();
    let has_more = sessions.len() > limit;
    sessions.truncate(limit);
    let next_cursor = has_more.then(|| (offset + sessions.len()).to_string());
    let registry = state.registry.read().await;
    let rows: Vec<Value> = sessions
        .into_iter()
        .map(|session| {
            let online = registry
                .sessions
                .get(&session.id)
                .map(|runtime| runtime.online)
                .unwrap_or(session.online);
            catalog_row(&session, online)
        })
        .collect();
    json!({
        "channel": "sessions",
        "event": "catalog",
        "data": {
            "activeSessionId": active,
            "sessions": rows,
            "nextCursor": next_cursor,
            "hasMore": has_more,
        }
    })
}

/// One `sessions/catalog` row. The PWA reads `lastActivityAt` and
/// `hasPendingUiRequest` (`sheets-view.js`), so both are always present:
/// `lastActivityAt` falls back to the relay's `updatedAt`, and
/// `hasPendingUiRequest` defaults to false when the agent does not report it.
fn catalog_row(session: &crate::protocol::SessionRow, online: bool) -> Value {
    let model = match (session.model_provider.clone(), session.model_id.clone()) {
        (Some(provider), id) => Some(json!({
            "provider": provider,
            "id": id,
            "name": session.model_name.clone(),
        })),
        _ => None,
    };
    json!({
        "id": session.id,
        "deviceId": session.device_id,
        "localId": session.local_id,
        "sessionId": session.session_id,
        "label": session.label,
        "kind": session.kind,
        "cwd": session.cwd,
        "model": model,
        "thinkingLevel": session.thinking_level,
        "isStreaming": session.is_streaming,
        "isCompacting": session.is_compacting,
        "hasPendingUiRequest": session.has_pending_ui_request.unwrap_or(false),
        "messageCount": session.message_count,
        "lastError": session.last_error,
        "online": online,
        "lastActivityAt": session.last_activity_at.unwrap_or(session.updated_at),
        "updatedAt": session.updated_at,
    })
}

fn descriptor_from_status(payload: &Value, local_id: &str) -> Option<SessionDescriptor> {
    if payload.get("channel").and_then(Value::as_str) != Some("server")
        || payload.get("event").and_then(Value::as_str) != Some("status")
    {
        return None;
    }
    let data = payload.get("data")?;
    let model = data.get("model").and_then(|model| {
        let provider = model
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let id = model
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let name = model.get("name").and_then(Value::as_str).map(str::to_owned);
        (!provider.is_empty() || !id.is_empty()).then_some(ModelDescriptor { provider, id, name })
    });
    Some(SessionDescriptor {
        id: local_id.to_owned(),
        session_id: data
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        has_pending_ui_request: data.get("hasPendingUiRequest").and_then(Value::as_bool),
        last_activity_at: data.get("lastActivityAt").and_then(Value::as_i64),
        label: data
            .get("sessionName")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| data.get("cwd").and_then(Value::as_str).map(str::to_owned)),
        kind: None,
        cwd: data.get("cwd").and_then(Value::as_str).map(str::to_owned),
        model,
        thinking_level: data
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_owned),
        is_streaming: data.get("isStreaming").and_then(Value::as_bool),
        is_compacting: data.get("isCompacting").and_then(Value::as_bool),
        message_count: data.get("messageCount").and_then(Value::as_u64),
        last_error: data
            .get("lastError")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    })
}

fn server_error(code: &str, message: &str) -> Arc<str> {
    json_line(json!({
        "channel": "server",
        "event": "error",
        "data": { "code": code, "message": message }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionRow;

    fn row(session_id: Option<&str>, pending: Option<bool>, last: Option<i64>) -> SessionRow {
        SessionRow {
            id: "device-a:parent-session".to_owned(),
            user_id: "user-a".to_owned(),
            device_id: "device-a".to_owned(),
            local_id: "parent-session".to_owned(),
            session_id: session_id.map(str::to_owned),
            has_pending_ui_request: pending,
            last_activity_at: last,
            label: Some("CLI session".to_owned()),
            kind: "parent".to_owned(),
            cwd: Some("/work".to_owned()),
            model_provider: Some("mock".to_owned()),
            model_id: Some("mock-1".to_owned()),
            model_name: Some("Mock".to_owned()),
            thinking_level: Some("medium".to_owned()),
            is_streaming: false,
            is_compacting: false,
            message_count: 3,
            last_error: None,
            online: true,
            created_at: 1000,
            updated_at: 2000,
            last_seen_at: Some(2000),
            deleted_at: None,
        }
    }

    #[test]
    fn catalog_row_carries_pwa_fields_and_falls_back_to_updated_at() {
        let with_agent = catalog_row(&row(Some("native-session"), Some(true), Some(1500)), true);
        assert_eq!(with_agent["sessionId"], "native-session");
        assert_eq!(with_agent["hasPendingUiRequest"], true);
        assert_eq!(with_agent["lastActivityAt"], 1500);

        // Older agent descriptors omit the fields: the row must still be
        // renderable (PWA reads these directly).
        let fallback = catalog_row(&row(None, None, None), false);
        assert!(fallback["sessionId"].is_null());
        assert_eq!(fallback["hasPendingUiRequest"], false);
        assert_eq!(fallback["lastActivityAt"], 2000);
    }

    #[test]
    fn descriptor_from_status_picks_up_pwa_fields() {
        let status = json!({
            "channel": "server",
            "event": "status",
            "data": {
                "sessionId": "native-session",
                "sessionName": "Mock session",
                "hasPendingUiRequest": true,
                "lastActivityAt": 1729000000000i64,
                "isStreaming": false
            }
        });
        let descriptor = descriptor_from_status(&status, "parent-session").expect("status");
        assert_eq!(descriptor.session_id.as_deref(), Some("native-session"));
        assert_eq!(descriptor.has_pending_ui_request, Some(true));
        assert_eq!(descriptor.last_activity_at, Some(1729000000000));
    }

    #[test]
    fn extension_ui_response_keeps_dialog_id_and_uses_request_id_for_correlation() {
        let mut dialog = json!({
            "kind": "rpc",
            "command": {"type": "extension_ui_response", "id": "ui-1", "confirmed": true},
        });
        assert_eq!(
            apply_forward_id(&mut dialog, "f-7-1").as_deref(),
            Some("ui-1")
        );
        assert_eq!(dialog["command"]["id"], "ui-1");
        assert_eq!(dialog["requestId"], "f-7-1");

        let mut prompt = json!({
            "kind": "rpc",
            "command": {"type": "prompt", "id": "p-1", "message": "hi"},
        });
        assert_eq!(
            apply_forward_id(&mut prompt, "f-7-2").as_deref(),
            Some("p-1")
        );
        assert_eq!(prompt["command"]["id"], "f-7-2");
        assert!(prompt.get("requestId").is_none());
    }
}
