//! Strict, definite-length CBOR helpers used by the protocol.
mod decoder;
mod encoder;
mod options;

pub use decoder::decode_value;
pub use encoder::encode_cbor;
pub(crate) use encoder::encode_cbor_framed;
pub use options::{
    CborError, CborOptions, DEFAULT_MAX_CBOR_BYTE_LENGTH, DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
    DEFAULT_MAX_CBOR_DEPTH,
};
