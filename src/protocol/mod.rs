//! Protocol types, strict CBOR encoding, and incremental framing.

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
