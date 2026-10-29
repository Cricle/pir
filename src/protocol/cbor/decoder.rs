//! Strict CBOR decoder using the `ciborium` library with post-decoding
//! validation for the upstream protocol subset.
//!
//! `ciborium` handles the wire format; we validate the decoded value tree
//! against the strict rules: no tags, no indefinite-length (already rejected
//! by ciborium's definite-length encoding), no duplicate map keys, no
//! non-string map keys, finite numbers only, safe integer range, and
//! configurable depth/container/byte limits.

use std::io::Cursor;

use super::options::{CborError, CborOptions};

/// Decode a single strict CBOR value from `bytes`, validating the protocol
/// subset. Returns the decoded value as `serde_json::Value`.
pub fn decode_value(bytes: &[u8], options: CborOptions) -> Result<serde_json::Value, CborError> {
    let options = options.validate()?;
    if bytes.len() > options.max_byte_length {
        return Err(CborError::ByteLimit(options.max_byte_length));
    }

    validate_wire_shape(bytes, options)?;

    let mut cursor = Cursor::new(bytes);
    let value: ciborium::value::Value = ciborium::de::from_reader(&mut cursor)
        .map_err(|e| CborError::Serialization(e.to_string()))?;

    // Check for trailing data: after decoding one value, the cursor must be
    // at the end of the input.
    let consumed = cursor.position() as usize;
    if consumed != bytes.len() {
        return Err(CborError::Malformed(
            "payload contains trailing data".into(),
        ));
    }

    validate_value(&value, 0, options)?;
    let json = serde_json::to_value(&value).map_err(|e| CborError::Serialization(e.to_string()))?;
    Ok(json)
}

/// Validate a decoded `ciborium::value::Value` against the strict protocol
/// subset. This step is necessary because `ciborium`'s decoder accepts a
/// superset of what the protocol allows (tags, non-string map keys, etc.).
/// Reject wire encodings outside the protocol's canonical subset before
/// handing bytes to ciborium. Ciborium accepts indefinite containers and
/// short floating-point encodings, while the upstream decoder does not.
/// Nesting depth is bounded here (before ciborium's own recursive decoder
/// runs) so hostile frames fail with [`CborError::DepthLimit`] instead of
/// overflowing the stack.
fn validate_wire_shape(bytes: &[u8], options: CborOptions) -> Result<(), CborError> {
    fn read_argument(bytes: &[u8], offset: &mut usize, additional: u8) -> Result<u64, CborError> {
        let width = match additional {
            n @ 0..=23 => return Ok(u64::from(n)),
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            _ => {
                return Err(CborError::Malformed(
                    "indefinite-length CBOR items are not supported".into(),
                ))
            }
        };
        if bytes.len().saturating_sub(*offset) < width {
            return Err(CborError::Malformed("truncated CBOR payload".into()));
        }
        let mut value = 0u64;
        for byte in &bytes[*offset..*offset + width] {
            value = (value << 8) | u64::from(*byte);
        }
        *offset += width;
        Ok(value)
    }

    fn item(
        bytes: &[u8],
        offset: &mut usize,
        depth: usize,
        max_depth: usize,
    ) -> Result<(), CborError> {
        if depth > max_depth {
            return Err(CborError::DepthLimit(max_depth));
        }
        let initial = *bytes
            .get(*offset)
            .ok_or_else(|| CborError::Malformed("truncated CBOR payload".into()))?;
        *offset += 1;
        let major = initial >> 5;
        let additional = initial & 0x1f;
        match major {
            0 | 1 => {
                let _ = read_argument(bytes, offset, additional)?;
            }
            // The v8 protocol has no byte-string fields. Accepting them here
            // is a remote amplification vector: ciborium materializes the
            // bytes and `serde_json::to_value` expands every byte into a
            // separate Value before schema validation.
            2 => {
                return Err(CborError::Malformed(
                    "CBOR byte strings are not supported".into(),
                ))
            }
            3 => {
                let length = read_argument(bytes, offset, additional)? as usize;
                if bytes.len().saturating_sub(*offset) < length {
                    return Err(CborError::Malformed("truncated CBOR payload".into()));
                }
                *offset += length;
            }
            4 => {
                let length = read_argument(bytes, offset, additional)? as usize;
                for _ in 0..length {
                    item(bytes, offset, depth + 1, max_depth)?;
                }
            }
            5 => {
                let length = read_argument(bytes, offset, additional)? as usize;
                for _ in 0..length {
                    item(bytes, offset, depth + 1, max_depth)?;
                    item(bytes, offset, depth + 1, max_depth)?;
                }
            }
            6 => return Err(CborError::Malformed("tags are not supported".into())),
            7 => match additional {
                20..=22 => {}
                27 => {
                    if bytes.len().saturating_sub(*offset) < 8 {
                        return Err(CborError::Malformed("truncated CBOR payload".into()));
                    }
                    *offset += 8;
                }
                _ => {
                    return Err(CborError::Malformed(
                        "unsupported CBOR simple value or floating-point width".into(),
                    ))
                }
            },
            _ => return Err(CborError::Malformed("unsupported CBOR major type".into())),
        }
        Ok(())
    }

    let mut offset = 0;
    item(bytes, &mut offset, 0, options.max_depth)?;
    if offset != bytes.len() {
        return Err(CborError::Malformed(
            "payload contains trailing data".into(),
        ));
    }
    Ok(())
}

fn validate_value(
    value: &ciborium::value::Value,
    depth: usize,
    options: CborOptions,
) -> Result<(), CborError> {
    if depth > options.max_depth {
        return Err(CborError::DepthLimit(options.max_depth));
    }
    match value {
        ciborium::value::Value::Null | ciborium::value::Value::Bool(_) => {}
        ciborium::value::Value::Integer(n) => {
            let i128 = i128::from(*n);
            // JavaScript safe integer range.
            let min = -9_007_199_254_740_991i128;
            let max = 9_007_199_254_740_991i128;
            if i128 < min || i128 > max {
                return Err(CborError::Malformed(
                    "integer is outside the safe range".into(),
                ));
            }
        }
        ciborium::value::Value::Float(f) => {
            if !f.is_finite() {
                return Err(CborError::Malformed("numbers must be finite".into()));
            }
            // Also check safe integer range for integer-valued floats.
            if f.fract() == 0.0 && f.abs() > 9_007_199_254_740_991f64 {
                return Err(CborError::Malformed(
                    "integer is outside the safe range".into(),
                ));
            }
        }
        ciborium::value::Value::Text(s) => {
            if s.len() > options.max_byte_length {
                return Err(CborError::ByteLimit(options.max_byte_length));
            }
        }
        ciborium::value::Value::Bytes(_) => {
            return Err(CborError::Malformed(
                "CBOR byte strings are not supported".into(),
            ))
        }
        ciborium::value::Value::Array(items) => {
            if items.len() > options.max_container_length {
                return Err(CborError::ContainerLimit(options.max_container_length));
            }
            for item in items {
                validate_value(item, depth + 1, options)?;
            }
        }
        ciborium::value::Value::Map(entries) => {
            if entries.len() > options.max_container_length {
                return Err(CborError::ContainerLimit(options.max_container_length));
            }
            // Borrowed keys: the set only answers "seen already?" while this
            // map is validated, so cloning every key into the set (a heap
            // allocation per key) is pure waste.
            let mut keys = std::collections::HashSet::with_capacity(entries.len());
            for (key, val) in entries {
                match key {
                    ciborium::value::Value::Text(k) => {
                        if !keys.insert(k.as_str()) {
                            return Err(CborError::Malformed(
                                "map contains a duplicate key".into(),
                            ));
                        }
                    }
                    _ => {
                        return Err(CborError::Malformed("map keys must be strings".into()));
                    }
                }
                validate_value(val, depth + 1, options)?;
            }
        }
        ciborium::value::Value::Tag(_, _) => {
            return Err(CborError::Malformed("tags are not supported".into()));
        }
        _ => {
            return Err(CborError::Malformed("unsupported CBOR value type".into()));
        }
    }
    Ok(())
}
