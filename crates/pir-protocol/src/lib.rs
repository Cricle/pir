//! Provider-agnostic wire protocol for pir.
//!
//! This crate owns the engine-independent half of the client/server wire: the
//! strict CBOR subset codec, the four-byte length-prefixed framing, and the
//! schema validators for every envelope the protocol carries. It depends on
//! `serde` / `serde_json` / `ciborium` / `thiserror` only — never on `pir-core`.
//!
//! A host integrates the protocol by encoding/decoding against these types; the
//! connection state machine, the session host and the RPC dispatch stay
//! host-side.
//!
//! The module tree is unchanged from its `pir-core/src/protocol/` layout, and
//! `pir-core` re-exports this crate as `pub use pir_protocol as protocol;`, so
//! every `crate::protocol::…` and `pir::protocol::…` path still resolves.

#![deny(unsafe_code)]

pub mod cbor;
pub mod codec;
pub mod framing;
pub mod schemas;

pub use cbor::{decode_value, encode_cbor, CborError, CborOptions};
pub use codec::{
    encode_client_envelope, encode_client_value, encode_server_envelope, parse_client_value,
    ClientMessageDecoder, ProtocolError, ServerMessageDecoder,
};
pub use framing::{encode_frame, FrameDecoder, FrameError, FrameOptions, DEFAULT_MAX_FRAME_LENGTH};
pub use schemas::*;
