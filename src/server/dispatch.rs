//! Request routing for the control and session services.

use serde_json::Value;
use uuid::Uuid;

use crate::protocol::schemas::field_opt;
use crate::protocol::{
    AttachmentFrame, ModelRef, ResponseFrame, RpcTarget, ServerEnvelope, ServiceCall,
    SERVICE_CONTROL_ID, SESSION_SERVICE_ID,
};

use super::connection::{send_envelope, ConnectionCtx};
use super::session::SessionCommandError;
use super::ServerError;

const RESPONSE_TYPE: &str = "response";
const ATTACHMENT_TYPE: &str = "attachment";

const MEMBER_LIST: &str = "list";
const MEMBER_CREATE: &str = "create";
const MEMBER_ATTACH: &str = "attach";
const MEMBER_DETACH: &str = "detach";
const MEMBER_PROMPT: &str = "prompt";
const MEMBER_STEER: &str = "steer";
const MEMBER_ABORT: &str = "abort";
const MEMBER_SET_MODEL: &str = "set_model";
const MEMBER_SET_THINKING: &str = "set_thinking";
/// pir extension (upstream reaches `SessionRouter.removeSession` only through
/// internal plumbing; the v8 wire protocol frames no removeSession command, so
/// the member is added without touching `PROTOCOL_VERSION`).
const MEMBER_REMOVE_SESSION: &str = "remove_session";

pub(super) async fn handle_request(
    ctx: &ConnectionCtx<'_>,
    value: &Value,
) -> Result<(), ServerError> {
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        // A request without a string id can never be matched by a client;
        // answering with a typed error is better than dropping it silently.
        return send_envelope(
            ctx.frame_tx,
            &error_response(
                "",
                &RequestError::invalid_request("request requires a string id"),
            ),
            ctx.options,
        );
    };
    match route_request(ctx, value).await {
        Ok(RouteOutcome::Deferred) => {
            // The prompt path: the response frame is delivered by the run
            // task when the prompt finishes (same response id). Returning
            // now keeps the read loop free so abort/steer/cancel frames are
            // processed while the run is in flight.
            return Ok(());
        }
        Ok(RouteOutcome::Inline { result, attachment }) => {
            if let Some(attachment) = attachment {
                let envelope = ServerEnvelope::Attachment(AttachmentFrame {
                    type_: ATTACHMENT_TYPE.to_owned(),
                    attachment,
                });
                // An invalid attachment frame is a server bug; close the
                // connection rather than answering without the route update.
                send_envelope(ctx.frame_tx, &envelope, ctx.options)?;
            }
            send_envelope(ctx.frame_tx, &ok_response(id, result), ctx.options)?;
        }
        Err(error) => {
            // A response that cannot be delivered must close the connection:
            // silently continuing would leave the client waiting forever.
            send_envelope(ctx.frame_tx, &error_response(id, &error), ctx.options)?;
        }
    }
    Ok(())
}

/// Routing outcome. `Inline` answers right away: the response result plus an
/// optional out-of-band attachment route update (`None` inner value =
/// detached, no session route). `Deferred` marks the prompt path: no frame is
/// sent inline; the run task sends the response on completion.
enum RouteOutcome {
    Inline {
        result: Value,
        attachment: Option<Option<RpcTarget>>,
    },
    Deferred,
}

impl RouteOutcome {
    fn result(result: Value) -> Self {
        Self::Inline {
            result,
            attachment: None,
        }
    }

    fn with_attachment(result: Value, attachment: Option<RpcTarget>) -> Self {
        Self::Inline {
            result,
            attachment: Some(attachment),
        }
    }

    fn deferred() -> Self {
        Self::Deferred
    }
}

/// Handle one `cancel` frame. Upstream `Server.handleCancel`
/// (server.ts:298-304): the frame's `id` is the id of the request to
/// cancel — the active request registered under that id with an identical
/// target (upstream `sameTarget`) is aborted, and the cancel frame itself
/// NEVER produces a response. The cancelled request answers for itself:
/// when its run settles it reports `{ok:false, error:{code:"cancelled"}}`
/// (server.ts:394-400). Sending an ack for the cancel id instead would put
/// two responses on one id — an upstream client fails the whole connection
/// when a response has no matching request (client.ts:333-337) — and a
/// stale/unknown id is silently ignored, exactly like the upstream
/// `activeRequests.get(id)` lookup miss.
pub(super) async fn handle_cancel(
    ctx: &ConnectionCtx<'_>,
    value: &Value,
) -> Result<(), ServerError> {
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(target_value) = value.get("target") else {
        return Ok(());
    };
    // A target that cannot name this server's session cancels nothing
    // (upstream ignores those silently).
    let session = match crate::protocol::schemas::parse_rpc_target(target_value, ".target") {
        Ok(RpcTarget::Session(session)) if session.server_id == ctx.state.server_id() => session,
        _ => return Ok(()),
    };
    // Only a request still registered as active can be cancelled; a stale
    // id (request already settled) or a target differing from the request's
    // own target is ignored (upstream `activeRequests.get(id)` +
    // `sameTarget`).
    let target = RpcTarget::Session(session.clone());
    let active_target = ctx.active.read().await.get(id).cloned();
    if active_target.as_ref() != Some(&target) {
        return Ok(());
    }
    // `state.cancel` aborts the matched run and marks it cancelled, so the
    // run task answers the REQUEST's id with the `cancelled` error — no
    // response is sent for the cancel frame itself.
    let _ = ctx
        .state
        .cancel(&session.session_id, &session.attachment_id)
        .await;
    Ok(())
}

/// A routing-level error mapped onto protocol error codes.
struct RequestError {
    code: &'static str,
    message: String,
}

impl RequestError {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "not_found",
            message: message.into(),
        }
    }
}

impl From<SessionCommandError> for RequestError {
    fn from(error: SessionCommandError) -> Self {
        let (code, message) = match error {
            SessionCommandError::NotFound => ("not_found", "session not found".to_owned()),
            SessionCommandError::Limit => ("invalid_request", "session limit reached".to_owned()),
            SessionCommandError::Locked => (
                "session_locked",
                "session is owned by another connection".to_owned(),
            ),
            SessionCommandError::Busy => ("busy", "session is busy".to_owned()),
            SessionCommandError::Draining => ("server_draining", "server is draining".to_owned()),
            // Upstream's exact cancelled-request vocabulary (server.ts:397):
            // the aborted request's own terminal response.
            SessionCommandError::Cancelled => ("cancelled", "RPC request cancelled".to_owned()),
            SessionCommandError::Invalid(message) => ("invalid_request", message),
            SessionCommandError::Provider(_) => (
                "internal_error",
                "request failed; server details are not available".to_owned(),
            ),
            SessionCommandError::Storage(_) => (
                "internal_error",
                "session removal could not complete".to_owned(),
            ),
            SessionCommandError::Policy => ("forbidden", "remote execution is disabled".to_owned()),
        };
        Self { code, message }
    }
}

/// Route one v8 request to the control or session service.
async fn route_request(
    ctx: &ConnectionCtx<'_>,
    value: &Value,
) -> Result<RouteOutcome, RequestError> {
    let target_value = value
        .get("target")
        .ok_or_else(|| RequestError::invalid_request("request requires target and call"))?;
    let target = crate::protocol::schemas::parse_rpc_target(target_value, ".target")
        .map_err(|error| RequestError::invalid_request(error.to_string()))?;
    let call_value = value
        .get("call")
        .ok_or_else(|| RequestError::invalid_request("request requires target and call"))?;
    let call = crate::protocol::schemas::parse_service_call(call_value)
        .map_err(|error| RequestError::invalid_request(error.to_string()))?;

    match call.service_id.as_str() {
        SERVICE_CONTROL_ID => route_control_call(ctx, target, call)
            .await
            .map(RouteOutcome::result),
        SESSION_SERVICE_ID => {
            route_session_call(ctx, target, call, value.get("id").and_then(Value::as_str)).await
        }
        _ => Err(RequestError::not_found("unknown service")),
    }
}

/// Route a `$chord.service` control call.
async fn route_control_call(
    ctx: &ConnectionCtx<'_>,
    target: RpcTarget,
    call: ServiceCall,
) -> Result<Value, RequestError> {
    if target.server_id() != ctx.state.server_id() {
        return Err(RequestError::not_found("unknown serverId"));
    }
    if !matches!(target, RpcTarget::Server(_)) {
        return Err(RequestError::invalid_request(
            "the control service requires a server target",
        ));
    }
    match crate::protocol::schemas::decode_service_control_call(&call) {
        Some(crate::protocol::schemas::ServiceControlCall::Catalogue) => Ok(serde_json::json!([
            {"serviceId": SESSION_SERVICE_ID, "mode": "singleton"}
        ])),
        Some(crate::protocol::schemas::ServiceControlCall::Subscribe {
            subscription_id,
            service_id,
            mode,
        }) => {
            if service_id != SESSION_SERVICE_ID {
                return Err(RequestError::not_found("unknown service"));
            }
            // Every subscription amplifies each broadcast into one frame for
            // this connection; cap the count per connection. Re-subscribing
            // an existing subscription id replaces it and is always allowed.
            let mut subscriptions = ctx.subscriptions.write().await;
            if !subscriptions.contains_key(&subscription_id)
                && subscriptions.len() >= ctx.security.max_subscriptions
            {
                return Err(RequestError::invalid_request(format!(
                    "subscription limit reached (max {})",
                    ctx.security.max_subscriptions
                )));
            }
            subscriptions.insert(subscription_id, service_id.clone());
            Ok(serde_json::json!({
                "serviceId": service_id,
                "mode": mode.as_str(),
                "instances": [],
            }))
        }
        Some(crate::protocol::schemas::ServiceControlCall::Unsubscribe { subscription_id }) => {
            ctx.subscriptions.write().await.remove(&subscription_id);
            Ok(Value::Null)
        }
        None => Err(RequestError::not_found("unknown service member")),
    }
}

/// Route a `pir.session` call, applying attachment fencing for session targets.
/// `request_id` (already validated by the request handler) is echoed in the
/// deferred prompt response.
async fn route_session_call(
    ctx: &ConnectionCtx<'_>,
    target: RpcTarget,
    call: ServiceCall,
    request_id: Option<&str>,
) -> Result<RouteOutcome, RequestError> {
    match target {
        RpcTarget::Server(target) => {
            if target.server_id != ctx.state.server_id() {
                return Err(RequestError::not_found("unknown serverId"));
            }
            match call.member.as_str() {
                MEMBER_LIST => {
                    if !call.args.is_empty() {
                        return Err(RequestError::invalid_request("list takes no arguments"));
                    }
                    let meta = ctx.state.session_metadata().await;
                    Ok(RouteOutcome::result(
                        serde_json::json!({ "command": MEMBER_LIST, "sessions": meta }),
                    ))
                }
                MEMBER_CREATE => {
                    let args = optional_args_object(&call)?;
                    let cwd = args
                        .and_then(|o| field_opt(o, "cwd"))
                        .and_then(Value::as_str)
                        .map(|s| s.to_owned());
                    // A remote client naming an arbitrary cwd points the
                    // whole agent (tools, context files, settings) at any
                    // path on the host; the security policy gates it.
                    if let Some(cwd) = &cwd {
                        if !ctx.security.allows_cwd(cwd) {
                            return Err(RequestError::from(SessionCommandError::Policy));
                        }
                    }
                    let name = args
                        .and_then(|o| field_opt(o, "name"))
                        .and_then(Value::as_str)
                        .map(|s| s.to_owned());
                    // Shared extraction: a missing/non-string provider or id
                    // falls back to "default" (as the session model default).
                    let model = args
                        .and_then(|o| field_opt(o, "model"))
                        .and_then(ModelRef::from_value);
                    let thinking_level = args
                        .and_then(|o| field_opt(o, "thinkingLevel"))
                        .and_then(Value::as_str)
                        .map(|s| s.to_owned());
                    let session_id_for_create = Uuid::new_v4().to_string();
                    let timestamp = crate::ai::util::now_ms();
                    let snapshot = ctx
                        .state
                        .create_session(
                            session_id_for_create,
                            cwd,
                            name,
                            model,
                            thinking_level,
                            timestamp,
                        )
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::result(serde_json::json!({
                        "command": MEMBER_CREATE,
                        "session": snapshot,
                    })))
                }
                MEMBER_ATTACH => {
                    let args = call
                        .args
                        .first()
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            RequestError::invalid_request("attach requires args[0] with sessionId")
                        })?;
                    let session_id = field_opt(args, "sessionId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            RequestError::invalid_request("attach requires sessionId")
                        })?;
                    let snapshot = ctx
                        .state
                        .attach_session(session_id, ctx.connection_id)
                        .await
                        .map_err(RequestError::from)?;
                    // The leased attachment id is this connection's id; the
                    // out-of-band attachment frame carries the session route.
                    Ok(RouteOutcome::with_attachment(
                        serde_json::json!({
                            "command": MEMBER_ATTACH,
                            "session": snapshot,
                            "attachmentId": ctx.connection_id,
                        }),
                        Some(RpcTarget::session(
                            ctx.state.server_id(),
                            session_id,
                            ctx.connection_id,
                        )),
                    ))
                }
                _ => Err(RequestError::not_found("unknown service member")),
            }
        }
        RpcTarget::Session(target) => {
            if target.server_id != ctx.state.server_id() {
                return Err(RequestError::not_found("unknown serverId"));
            }
            let session_id = target.session_id.clone();
            let attachment_id = target.attachment_id.clone();
            match call.member.as_str() {
                MEMBER_ATTACH => Err(RequestError::invalid_request(
                    "attach requires a server target",
                )),
                MEMBER_DETACH => {
                    let session = ctx
                        .state
                        .detach_session(&session_id, &attachment_id)
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::with_attachment(
                        serde_json::json!({
                            "command": MEMBER_DETACH,
                            "sessionId": session.id,
                        }),
                        None,
                    ))
                }
                MEMBER_PROMPT => {
                    if !ctx.security.allow_remote_agent {
                        return Err(RequestError::from(SessionCommandError::Policy));
                    }
                    let args = call
                        .args
                        .first()
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            RequestError::invalid_request("prompt requires args[0] with text")
                        })?;
                    let text = field_opt(args, "text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| RequestError::invalid_request("prompt requires text"))?;
                    // The run happens in a spawned per-session task; the
                    // response frame is delivered by that task when the run
                    // finishes (same response id), keeping the read loop free
                    // so abort/steer/cancel remain reachable mid-run.
                    let (finished, receiver) = tokio::sync::oneshot::channel();
                    ctx.state
                        .prompt(&session_id, &attachment_id, text.to_owned(), finished)
                        .await
                        .map_err(RequestError::from)?;
                    let frame_tx = ctx.frame_tx.clone();
                    let options = ctx.options;
                    let shutdown = std::sync::Arc::clone(ctx.shutdown);
                    // The handler only routes id-bearing requests; the
                    // fallback is unreachable for the deferred response.
                    let response_id = request_id.unwrap_or_default().to_owned();
                    // Register the in-flight run as this connection's active
                    // request (upstream `state.activeRequests`): a `cancel`
                    // frame naming this id may cancel exactly this run. The
                    // entry is dropped as soon as the run settles, before the
                    // terminal response is sent, so a stale id can never
                    // cancel a later run.
                    let active = std::sync::Arc::clone(ctx.active);
                    active.write().await.insert(
                        response_id.clone(),
                        RpcTarget::session(
                            ctx.state.server_id(),
                            session_id.as_str(),
                            attachment_id.as_str(),
                        ),
                    );
                    tokio::spawn(async move {
                        let outcome = receiver.await;
                        active.write().await.remove(&response_id);
                        let Ok(outcome) = outcome else {
                            // The run task vanished without a result (only
                            // possible if the connection is already gone).
                            return;
                        };
                        let envelope = match outcome {
                            Ok(session) => ok_response(
                                &response_id,
                                serde_json::json!({
                                    "command": MEMBER_PROMPT,
                                    "session": session,
                                }),
                            ),
                            Err(error) => error_response(&response_id, &RequestError::from(error)),
                        };
                        // An undeliverable response must never be lost
                        // silently: a full channel means the client stopped
                        // reading, so disconnect the connection instead of
                        // leaving the client waiting for a response that will
                        // never come (a closed channel means the connection
                        // is already closing).
                        if let Err(error) = send_envelope(&frame_tx, &envelope, options) {
                            tracing::warn!(
                                %error,
                                "deferred prompt response could not be delivered; disconnecting"
                            );
                            shutdown.notify_one();
                        }
                    });
                    Ok(RouteOutcome::deferred())
                }
                MEMBER_STEER => {
                    let args = call
                        .args
                        .first()
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            RequestError::invalid_request("steer requires args[0] with text")
                        })?;
                    let text = field_opt(args, "text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| RequestError::invalid_request("steer requires text"))?;
                    let session = ctx
                        .state
                        .steer(&session_id, &attachment_id, text.to_owned())
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::result(serde_json::json!({
                        "command": MEMBER_STEER,
                        "session": session,
                    })))
                }
                MEMBER_ABORT => {
                    let session = ctx
                        .state
                        .abort(&session_id, &attachment_id)
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::result(serde_json::json!({
                        "command": MEMBER_ABORT,
                        "session": session,
                    })))
                }
                MEMBER_SET_MODEL => {
                    let args = call
                        .args
                        .first()
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            RequestError::invalid_request("set_model requires args[0] with model")
                        })?;
                    let model = field_opt(args, "model").and_then(ModelRef::from_value);
                    let Some(model) = model else {
                        return Err(RequestError::invalid_request("set_model requires model"));
                    };
                    let session = ctx
                        .state
                        .set_model(&session_id, &attachment_id, model)
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::result(serde_json::json!({
                        "command": MEMBER_SET_MODEL,
                        "session": session,
                    })))
                }
                MEMBER_SET_THINKING => {
                    let level = call
                        .args
                        .first()
                        .and_then(Value::as_object)
                        .and_then(|o| field_opt(o, "thinkingLevel"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            RequestError::invalid_request(
                                "set_thinking requires args[0] with thinkingLevel",
                            )
                        })?;
                    let session = ctx
                        .state
                        .set_thinking(&session_id, &attachment_id, level.to_owned())
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::result(serde_json::json!({
                        "command": MEMBER_SET_THINKING,
                        "session": session,
                    })))
                }
                MEMBER_REMOVE_SESSION => {
                    // The removal aborts the run, evicts the record, deletes
                    // the durable file, and broadcasts `closed` to
                    // subscribers. The out-of-band null attachment clears the
                    // caller's route, like upstream publishAttachment(undefined)
                    // when removeSession releases the attachments.
                    ctx.state
                        .remove_session(&session_id, &attachment_id)
                        .await
                        .map_err(RequestError::from)?;
                    Ok(RouteOutcome::with_attachment(
                        serde_json::json!({
                            "command": MEMBER_REMOVE_SESSION,
                            "sessionId": session_id,
                        }),
                        None,
                    ))
                }
                _ => Err(RequestError::not_found("unknown service member")),
            }
        }
    }
}

fn optional_args_object(
    call: &ServiceCall,
) -> Result<Option<&serde_json::Map<String, Value>>, RequestError> {
    match call.args.first() {
        None => Ok(None),
        Some(Value::Object(obj)) => Ok(Some(obj)),
        Some(_) => Err(RequestError::invalid_request(
            "args[0] must be an object when present",
        )),
    }
}

fn ok_response(id: &str, result: Value) -> ServerEnvelope {
    ServerEnvelope::Response(ResponseFrame {
        type_: RESPONSE_TYPE.to_owned(),
        id: id.to_owned(),
        ok: true,
        result: Some(result),
        error: None,
    })
}

fn error_response(id: &str, error: &RequestError) -> ServerEnvelope {
    ServerEnvelope::Response(ResponseFrame {
        type_: RESPONSE_TYPE.to_owned(),
        id: id.to_owned(),
        ok: false,
        result: None,
        error: Some(crate::protocol::schemas::ProtocolError {
            code: error.code.to_owned(),
            message: error.message.clone(),
            details: None,
        }),
    })
}
