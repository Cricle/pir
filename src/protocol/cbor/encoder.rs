//! Strict, definite-length CBOR encoder for `serde_json::Value`.
//!
//! The encoder is hand-rolled rather than routed through
//! `ciborium::ser::into_writer` because ciborium 0.2.2 shrinks floats to the
//! smallest lossless width (f16/f32, major-7 additional 25/26). The protocol's
//! decoder and upstream pi only accept additional 27 (`0xfb` + 8-byte
//! big-endian f64), so a ciborium-encoded float like `0.5` round-trips broken.
//! This encoder therefore emits every f64 as major-7 additional-27 and never
//! produces f16/f32 (or any tag/indefinite-length item).

use super::options::{CborError, CborOptions};

/// Strictly encode `value` in ONE traversal: every limit (depth, container
/// length, per-string byte length, finite/safe numbers) is checked at the
/// point the item is written, in the same depth-first order the former
/// separate validation pass used, so the first reported error and every
/// encoded byte are unchanged.
///
/// INVARIANT (single pass): the previous shape walked the whole value tree
/// with `validate_json_value` and then walked it again to write it — every
/// item visited twice, plus one `HashSet` allocated per object for a
/// duplicate-key check `serde_json::Value` cannot trigger (a `Map` holds
/// unique keys by construction, `preserve_order`'s `IndexMap` included).
/// Fusing the checks into the writer removes both costs; nothing about the
/// accepted subset or the error set changes.
pub fn encode_cbor(value: &serde_json::Value, options: CborOptions) -> Result<Vec<u8>, CborError> {
    let options = options.validate()?;
    let mut encoded = Vec::new();
    write_value(value, 0, options, &mut encoded)?;
    if encoded.len() > options.max_byte_length {
        return Err(CborError::ByteLimit(options.max_byte_length));
    }
    Ok(encoded)
}

/// [`encode_cbor`] that writes the CBOR payload BEHIND a four-byte frame
/// header the caller patches afterwards.
///
/// WHY (no payload copy): the protocol encoder used to encode the payload into
/// one `Vec` and then copy it into a second, header-prefixed `Vec` — a full
/// copy of every outgoing frame (up to the 16 MiB frame cap, e.g. session
/// snapshots). Reserving the header up front lets `write_value` append in
/// place, so the only buffer is the frame itself. The byte-limit check applies
/// to the payload bytes (header excluded), exactly like [`encode_cbor`].
pub(crate) fn encode_cbor_framed(
    value: &serde_json::Value,
    options: CborOptions,
) -> Result<Vec<u8>, CborError> {
    use crate::protocol::framing::FRAME_HEADER_LENGTH;

    let options = options.validate()?;
    let mut encoded = vec![0u8; FRAME_HEADER_LENGTH];
    write_value(value, 0, options, &mut encoded)?;
    if encoded.len() - FRAME_HEADER_LENGTH > options.max_byte_length {
        return Err(CborError::ByteLimit(options.max_byte_length));
    }
    Ok(encoded)
}

fn write_value(
    value: &serde_json::Value,
    depth: usize,
    options: CborOptions,
    out: &mut Vec<u8>,
) -> Result<(), CborError> {
    if depth > options.max_depth {
        return Err(CborError::DepthLimit(options.max_depth));
    }
    match value {
        serde_json::Value::Null => out.push(0xf6),
        serde_json::Value::Bool(flag) => out.push(if *flag { 0xf5 } else { 0xf4 }),
        serde_json::Value::Number(number) => {
            // Same checks the former validation pass applied (finite, and
            // integer-valued floats inside the JavaScript safe range).
            if let Some(n) = number.as_f64() {
                if !n.is_finite() {
                    return Err(CborError::Malformed("numbers must be finite".into()));
                }
                if n.fract() == 0.0 && n.abs() > 9_007_199_254_740_991_f64 {
                    return Err(CborError::Malformed(
                        "integer is outside the safe range".into(),
                    ));
                }
            }
            write_number(number, out)?;
        }
        serde_json::Value::String(text) => {
            if text.len() > options.max_byte_length {
                return Err(CborError::ByteLimit(options.max_byte_length));
            }
            write_head(out, 3, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        serde_json::Value::Array(items) => {
            if items.len() > options.max_container_length {
                return Err(CborError::ContainerLimit(options.max_container_length));
            }
            write_head(out, 4, items.len() as u64);
            for item in items {
                write_value(item, depth + 1, options, out)?;
            }
        }
        serde_json::Value::Object(entries) => {
            if entries.len() > options.max_container_length {
                return Err(CborError::ContainerLimit(options.max_container_length));
            }
            write_head(out, 5, entries.len() as u64);
            for (key, value) in entries {
                write_head(out, 3, key.len() as u64);
                out.extend_from_slice(key.as_bytes());
                write_value(value, depth + 1, options, out)?;
            }
        }
    }
    Ok(())
}

fn write_number(number: &serde_json::Number, out: &mut Vec<u8>) -> Result<(), CborError> {
    if let Some(unsigned) = number.as_u64() {
        write_head(out, 0, unsigned);
        return Ok(());
    }
    if let Some(signed) = number.as_i64() {
        // Major type 1 encodes -1 - n.
        let magnitude = signed.checked_add(1).unwrap_or(0).unsigned_abs();
        write_head(out, 1, magnitude);
        return Ok(());
    }
    // Every f64 (including integer-valued ones like 3.0) is emitted as
    // major-7 additional-27: `0xfb` + 8-byte big-endian IEEE 754 bits.
    // ciborium's writer would shrink this to f16/f32, which both pir's
    // decoder and upstream reject.
    let Some(float) = number.as_f64() else {
        return Err(CborError::Malformed(
            "number is outside the representable range".into(),
        ));
    };
    if !float.is_finite() {
        return Err(CborError::Malformed("numbers must be finite".into()));
    }
    out.push(0xfb);
    out.extend_from_slice(&float.to_bits().to_be_bytes());
    Ok(())
}

/// Write a definite-length item head: `major` + the argument in the minimal
/// width (inline 0-23, then 1/2/4/8-byte big-endian).
fn write_head(out: &mut Vec<u8>, major: u8, argument: u64) {
    let head = major << 5;
    if argument <= 23 {
        out.push(head | argument as u8);
    } else if argument <= 0xff {
        out.push(head | 24);
        out.push(argument as u8);
    } else if argument <= 0xffff {
        out.push(head | 25);
        out.extend_from_slice(&(argument as u16).to_be_bytes());
    } else if argument <= 0xffff_ffff {
        out.push(head | 26);
        out.extend_from_slice(&(argument as u32).to_be_bytes());
    } else {
        out.push(head | 27);
        out.extend_from_slice(&argument.to_be_bytes());
    }
}
