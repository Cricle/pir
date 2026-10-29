//! Server-to-client message types (v8 envelope).

use serde::Serialize;
use serde_json::Value;

use super::rpc::RpcTarget;
use super::types::{ProtocolError, TranscriptItem};

/// `hello_error` payload, shared by the v1 catalog and the v8 envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ServerHelloError {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "error")]
    pub error: ProtocolError,
}

/// v8 transcript progress variants (the concrete v1 event shapes, emitted as
/// the `pir.session` service update payload).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptProgress {
    ItemStarted {
        #[serde(rename = "item")]
        item: TranscriptItem,
    },
    AssistantDelta {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "contentIndex")]
        content_index: i64,
        #[serde(rename = "kind")]
        kind: String,
        #[serde(rename = "delta")]
        delta: String,
    },
    ItemUpdated {
        #[serde(rename = "item")]
        item: TranscriptItem,
    },
    ItemFinished {
        #[serde(rename = "item")]
        item: TranscriptItem,
    },
}

/// v8 server hello: `{type:"hello", version: 8, serverId}`.
#[derive(Debug, Clone, Serialize)]
pub struct ServerHelloV8 {
    #[serde(rename = "type")]
    pub type_: String,
    pub version: i64,
    #[serde(rename = "serverId")]
    pub server_id: String,
}

/// v8 response envelope: `{type:"response", id, ok, result?}` xor
/// `{type:"response", id, ok:false, error}`.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseFrame {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: String,
    pub ok: bool,
    #[serde(rename = "result", skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(rename = "error", skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

/// v8 service update push: `{type:"service_update", subscriptionId, update}`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceUpdateFrame {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "subscriptionId")]
    pub subscription_id: String,
    pub update: Value,
}

/// v8 out-of-band attachment update: `{type:"attachment", attachment}`.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentFrame {
    #[serde(rename = "type")]
    pub type_: String,
    pub attachment: Option<RpcTarget>,
}

/// v8 server message envelope.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ServerEnvelope {
    Hello(ServerHelloV8),
    HelloError(ServerHelloError),
    Response(ResponseFrame),
    ServiceUpdate(ServiceUpdateFrame),
    Attachment(AttachmentFrame),
}
