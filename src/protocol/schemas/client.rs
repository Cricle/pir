//! Client-to-server message types (v8 envelope).

use serde::Serialize;

use super::rpc::{RpcTarget, ServiceCall};

// ---------------------------------------------------------------------------
// Client / Server messages
// ---------------------------------------------------------------------------

/// `hello` is shared by the legacy v1 catalog and the v8 envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ClientHello {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "version")]
    pub version: i64,
}

// ---------------------------------------------------------------------------
// Protocol v8: typed envelopes for encoding
// ---------------------------------------------------------------------------

/// v8 client request envelope: `{type:"request", id, target, call}`.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: String,
    pub target: RpcTarget,
    pub call: ServiceCall,
}

/// v8 client cancel envelope: `{type:"cancel", id, target}`.
#[derive(Debug, Clone, Serialize)]
pub struct RpcCancel {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: String,
    pub target: RpcTarget,
}

/// v8 client message envelope (hello is shared with the legacy catalog).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ClientEnvelope {
    Hello(ClientHello),
    Request(RpcRequest),
    Cancel(RpcCancel),
}
