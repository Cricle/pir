//! Schema-validated CBOR and framed protocol message codec.

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use super::cbor::{decode_value, encode_cbor_framed, CborError, CborOptions};
use super::framing::{
    FrameDecoder, FrameError, FrameOptions, DEFAULT_MAX_FRAME_LENGTH, FRAME_HEADER_LENGTH,
};
use super::schemas::{
    parse_client_message, parse_server_message, ClientEnvelope, SchemaError, ServerEnvelope,
};

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("protocol validation failed: {0}")]
    Validation(#[from] SchemaError),
    #[error("CBOR codec failed: {0}")]
    Cbor(#[from] CborError),
    #[error("framing failed: {0}")]
    Framing(#[from] FrameError),
    #[error("JSON conversion failed: {0}")]
    Json(String),
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, ProtocolError> {
    serde_json::to_value(value).map_err(|error| ProtocolError::Json(error.to_string()))
}

fn cbor_to_json(bytes: &[u8], options: CborOptions) -> Result<Value, ProtocolError> {
    decode_value(bytes, options).map_err(ProtocolError::from)
}

/// Validate and frame one message for one protocol direction (the const bool
/// selects the client/server schema validator). The (validator, kind) pairing
/// lives once on `MessageDecoder`'s direction constants and is shared by every
/// public encoder below.
fn encode_protocol<const CLIENT: bool, T: Serialize>(
    value: &T,
    options: FrameOptions,
) -> Result<Vec<u8>, ProtocolError> {
    let json = to_json(value)?;
    (MessageDecoder::<CLIENT>::PARSE)(&json)?;
    let cbor_options = CborOptions {
        max_byte_length: options.max_frame_length,
        ..CborOptions::default()
    };
    // Encode the CBOR payload directly behind the frame header: the former
    // shape encoded to a payload `Vec` and copied it into a second framed
    // `Vec`, a full copy of every outgoing frame (up to 16 MiB).
    let mut frame = encode_cbor_framed(&json, cbor_options)?;
    let payload_len = frame.len() - FRAME_HEADER_LENGTH;
    if payload_len > options.max_frame_length {
        return Err(ProtocolError::Validation(SchemaError::new(format!(
            "unable to encode {}: frame exceeds configured limit",
            MessageDecoder::<CLIENT>::KIND
        ))));
    }
    let length = u32::try_from(payload_len)
        .map_err(|_| ProtocolError::Framing(FrameError::LengthOverflow))?;
    frame[..FRAME_HEADER_LENGTH].copy_from_slice(&length.to_be_bytes());
    Ok(frame)
}

/// Validate and encode one v8 client envelope.
pub fn encode_client_envelope(
    message: &ClientEnvelope,
    options: FrameOptions,
) -> Result<Vec<u8>, ProtocolError> {
    encode_protocol::<true, _>(message, options)
}

/// Validate and encode one v8 server envelope.
pub fn encode_server_envelope(
    message: &ServerEnvelope,
    options: FrameOptions,
) -> Result<Vec<u8>, ProtocolError> {
    encode_protocol::<false, _>(message, options)
}

/// Validate and encode an already-serialized client message value.
pub fn encode_client_value(value: &Value, options: FrameOptions) -> Result<Vec<u8>, ProtocolError> {
    encode_protocol::<true, _>(value, options)
}

/// Parse and schema-validate an already-decoded client message.
pub fn parse_client_value(value: &Value) -> Result<(), ProtocolError> {
    parse_client_message(value)?;
    Ok(())
}

/// One direction-parameterized frame decoder. Client- and server-bound
/// inbound streams share identical framing/CBOR machinery and differ only in
/// the schema validator applied to each decoded frame (the const bool).
pub struct MessageDecoder<const CLIENT: bool> {
    frames: FrameDecoder,
    cbor_options: CborOptions,
    failed: bool,
}

impl<const CLIENT: bool> MessageDecoder<CLIENT> {
    /// Schema validator for this decoder's direction.
    const PARSE: fn(&Value) -> Result<(), SchemaError> = if CLIENT {
        parse_client_message
    } else {
        parse_server_message
    };

    /// Diagnostic kind ("client"/"server") used in failure messages.
    const KIND: &'static str = if CLIENT { "client" } else { "server" };

    pub fn new(options: FrameOptions) -> Result<Self, FrameError> {
        let frames = FrameDecoder::new(options)?;
        Ok(Self {
            frames,
            cbor_options: CborOptions {
                max_byte_length: options.max_frame_length,
                ..CborOptions::default()
            },
            failed: false,
        })
    }

    pub fn with_default_limits() -> Self {
        Self {
            frames: FrameDecoder::with_default_limits(),
            cbor_options: CborOptions {
                max_byte_length: DEFAULT_MAX_FRAME_LENGTH,
                ..CborOptions::default()
            },
            failed: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, ProtocolError> {
        if self.failed {
            return Err(ProtocolError::Validation(SchemaError::new(format!(
                "{} message decoder has failed",
                Self::KIND
            ))));
        }
        let result = match self.frames.push(chunk) {
            Ok(frames) => frames
                .into_iter()
                .map(|frame| {
                    let value = cbor_to_json(&frame, self.cbor_options)?;
                    (Self::PARSE)(&value)?;
                    Ok(value)
                })
                .collect::<Result<Vec<_>, ProtocolError>>(),
            Err(error) => Err(ProtocolError::Framing(error)),
        };
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn end(&mut self) -> Result<(), ProtocolError> {
        if self.failed {
            return Err(ProtocolError::Validation(SchemaError::new(format!(
                "{} message decoder has failed",
                Self::KIND
            ))));
        }
        self.frames.end()?;
        Ok(())
    }
}

/// Frame decoder for inbound client messages (client schema validated).
pub type ClientMessageDecoder = MessageDecoder<true>;

/// Frame decoder for inbound server messages (server schema validated).
pub type ServerMessageDecoder = MessageDecoder<false>;
