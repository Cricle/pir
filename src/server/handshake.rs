//! Client preface authentication and the v8 hello handshake.

use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::protocol::{ServerEnvelope, ServerHelloError, ServerHelloV8, PROTOCOL_VERSION};

use super::connection::{send_envelope, ConnectionCtx};
use super::ServerError;

const HELLO_TYPE: &str = "hello";
const HELLO_ERROR_TYPE: &str = "hello_error";
const AUTH_PREFIX: &[u8] = b"PIR-AUTH ";
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Process one message received before the protocol greeting completes.
///
/// Returns `Ok(true)` once the connection is greeted and the message loop can
/// continue, and `Ok(false)` when the handshake failed and the connection must
/// be closed without further protocol responses.
pub(super) async fn handle_hello(
    ctx: &ConnectionCtx<'_>,
    value: &Value,
) -> Result<bool, ServerError> {
    let message_type = value.get("type").and_then(Value::as_str);
    match message_type {
        Some(HELLO_TYPE) => {
            let version = value.get("version").and_then(Value::as_i64);
            if version != Some(PROTOCOL_VERSION) {
                send_envelope(
                    ctx.frame_tx,
                    &ServerEnvelope::HelloError(ServerHelloError {
                        type_: HELLO_ERROR_TYPE.to_owned(),
                        error: protocol_error("version", "unsupported protocol version", None),
                    }),
                    ctx.options,
                )?;
                return Ok(false);
            }
            send_envelope(
                ctx.frame_tx,
                &ServerEnvelope::Hello(ServerHelloV8 {
                    type_: HELLO_TYPE.to_owned(),
                    version: PROTOCOL_VERSION,
                    server_id: ctx.state.server_id().to_owned(),
                }),
                ctx.options,
            )?;
            Ok(true)
        }
        _ => {
            send_envelope(
                ctx.frame_tx,
                &ServerEnvelope::HelloError(ServerHelloError {
                    type_: HELLO_ERROR_TYPE.to_owned(),
                    error: protocol_error(
                        "invalid_request",
                        "hello is required before requests",
                        None,
                    ),
                }),
                ctx.options,
            )?;
            Ok(false)
        }
    }
}

pub(super) async fn authenticate<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    token: &str,
) -> Result<(), ServerError> {
    let mut expected = Vec::with_capacity(AUTH_PREFIX.len() + token.len() + 1);
    expected.extend_from_slice(AUTH_PREFIX);
    expected.extend_from_slice(token.as_bytes());
    expected.push(b'\n');
    let result = tokio::time::timeout(AUTH_TIMEOUT, async {
        let mut received = Vec::with_capacity(expected.len());
        while received.len() < expected.len() {
            let mut byte = [0_u8; 1];
            if reader.read_exact(&mut byte).await.is_err() {
                return None;
            }
            received.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        Some(received)
    })
    .await;
    match result {
        Ok(Some(received)) if constant_time_eq(&expected, &received) => Ok(()),
        _ => Err(ServerError::Authentication),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn protocol_error(
    code: &str,
    message: &str,
    details: Option<Value>,
) -> crate::protocol::schemas::ProtocolError {
    crate::protocol::schemas::ProtocolError {
        code: code.to_owned(),
        message: message.to_owned(),
        details,
    }
}
