//! Round-trip and strict-rejection coverage for the extracted wire protocol.
//!
//! These exercise the public surface only (`pir_protocol::…`), so they pin the
//! behaviour the crate must keep after leaving `pir-core`: the frame codec is
//! byte-exact, the CBOR subset rejects everything the upstream protocol
//! forbids, and the schema validators accept the v8 envelope while rejecting
//! malformed ones.
//!
//! Every strict-CBOR rejection surfaces as [`CborError::Malformed`] with a
//! describing message (the wire pre-scan and the post-decode value walk both
//! report that way), so the assertions match the variant and the message
//! fragment the crate actually produces.

use serde_json::json;

use pir_protocol::framing::FRAME_HEADER_LENGTH;
use pir_protocol::{
    decode_value, encode_cbor, encode_frame, parse_client_message, parse_server_message, CborError,
    CborOptions, FrameDecoder, FrameError, FrameOptions,
};

/// Assert that decoding `bytes` fails with `Malformed` naming `fragment`.
fn assert_rejected(bytes: &[u8], fragment: &str) {
    match decode_value(bytes, CborOptions::default()) {
        Err(CborError::Malformed(message)) => assert!(
            message.contains(fragment),
            "expected a message containing {fragment:?}, got {message:?}"
        ),
        other => panic!("expected Malformed({fragment:?}), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn frame_prefixes_the_payload_with_its_big_endian_length() {
    let frame = encode_frame(b"hello").expect("encode");
    assert_eq!(frame.len(), FRAME_HEADER_LENGTH + 5);
    assert_eq!(&frame[..4], &[0, 0, 0, 5]);
    assert_eq!(&frame[4..], b"hello");
}

#[test]
fn frame_decoder_reassembles_chunked_payloads() {
    let mut decoder = FrameDecoder::with_default_limits();
    let frame = encode_frame(b"abcdefgh").expect("encode");

    // One byte at a time: nothing is emitted until the payload is complete.
    let mut emitted = Vec::new();
    for byte in frame {
        emitted.extend(decoder.push(&[byte]).expect("push"));
    }
    assert_eq!(emitted, vec![b"abcdefgh".to_vec()]);
}

#[test]
fn frame_decoder_splits_multiple_frames_from_one_chunk() {
    let mut decoder = FrameDecoder::with_default_limits();
    let mut stream = encode_frame(b"one").expect("encode");
    stream.extend(encode_frame(b"two").expect("encode"));

    let payloads = decoder.push(&stream).expect("push");
    assert_eq!(payloads, vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn frame_decoder_rejects_a_payload_over_the_configured_limit() {
    let mut decoder = FrameDecoder::new(FrameOptions {
        max_frame_length: 3,
    })
    .expect("decoder");
    let frame = encode_frame(b"toolong").expect("encode");
    assert!(matches!(
        decoder.push(&frame),
        Err(FrameError::Limit { .. })
    ));
}

#[test]
fn frame_options_reject_a_limit_that_does_not_fit_the_header() {
    assert!(matches!(
        FrameDecoder::new(FrameOptions {
            max_frame_length: u32::MAX as usize + 1,
        }),
        Err(FrameError::InvalidLimit)
    ));
}

// ---------------------------------------------------------------------------
// CBOR
// ---------------------------------------------------------------------------

#[test]
fn cbor_round_trips_a_nested_json_document() {
    let value = json!({
        "type": "request",
        "id": "abc",
        "nested": { "list": [1, true, null, "text"], "empty": {} },
    });
    let bytes = encode_cbor(&value, CborOptions::default()).expect("encode");
    let decoded = decode_value(&bytes, CborOptions::default()).expect("decode");
    assert_eq!(decoded, value);
}

#[test]
fn cbor_rejects_a_map_with_duplicate_keys() {
    // { "a": 1, "a": 2 } — ciborium would silently keep one; the strict walk
    // is what catches the duplication.
    let bytes = [0xa2, 0x61, b'a', 0x01, 0x61, b'a', 0x02];
    assert_rejected(&bytes, "duplicate key");
}

#[test]
fn cbor_rejects_non_string_map_keys() {
    // { 1: 2 }
    let bytes = [0xa1, 0x01, 0x02];
    assert_rejected(&bytes, "map keys must be strings");
}

#[test]
fn cbor_rejects_a_tagged_value() {
    // tag 0 (date/time) wrapping an integer.
    let bytes = [0xc0, 0x01];
    assert_rejected(&bytes, "tags are not supported");
}

#[test]
fn cbor_rejects_byte_strings() {
    // A two-byte byte string: the v8 protocol has no byte-string field, and
    // materializing one would amplify a hostile frame.
    let bytes = [0x42, 0x01, 0x02];
    assert_rejected(&bytes, "byte strings are not supported");
}

#[test]
fn cbor_rejects_trailing_data_after_a_complete_value() {
    // `1` followed by a stray byte.
    let bytes = [0x01, 0x02];
    assert_rejected(&bytes, "trailing data");
}

#[test]
fn cbor_rejects_payloads_over_the_byte_limit() {
    let value = json!("a string longer than the limit");
    assert!(matches!(
        encode_cbor(
            &value,
            CborOptions {
                max_byte_length: 4,
                ..CborOptions::default()
            }
        ),
        Err(CborError::ByteLimit(4))
    ));
}

#[test]
fn cbor_rejects_excessive_nesting() {
    // Nesting is bounded in the WIRE pre-scan, before ciborium's recursive
    // decoder runs, so a hostile frame fails with `DepthLimit` instead of
    // overflowing the stack. (serde_json cannot build the input: it caps its
    // own parser at 128 levels, so the payload is hand-encoded.)
    let depth = 20usize;
    // `depth` nested single-element arrays around the integer 1: major type 4,
    // one element each, then an int.
    let mut bytes = vec![0x81u8; depth];
    bytes.push(0x01);

    assert!(matches!(
        decode_value(
            &bytes,
            CborOptions {
                max_depth: 8,
                ..CborOptions::default()
            }
        ),
        Err(CborError::DepthLimit(8))
    ));
}

#[test]
fn cbor_rejects_an_integer_outside_the_javascript_safe_range() {
    // 2^53 encoded as a bare unsigned int (major 0, 8-byte argument).
    let too_big: u64 = 9_007_199_254_740_993;
    let mut bytes = vec![0x1b];
    bytes.extend_from_slice(&too_big.to_be_bytes());
    assert_rejected(&bytes, "safe range");
}

// ---------------------------------------------------------------------------
// Schema validation
// ---------------------------------------------------------------------------

#[test]
fn client_hello_and_cancel_are_accepted() {
    assert!(parse_client_message(&json!({ "type": "hello", "version": 8 })).is_ok());
    // A server-target cancel: `{ type, id, target }` with only `serverId`.
    let server_id = "0f8fad5b-d9cb-469f-a165-70867728950e";
    assert!(parse_client_message(&json!({
        "type": "cancel",
        "id": "1",
        "target": { "serverId": server_id },
    }))
    .is_ok());
}

#[test]
fn client_message_rejects_an_unknown_type_and_extra_fields() {
    assert!(parse_client_message(&json!({ "type": "nope" })).is_err());
    // `hello` is strict: an extra field is a schema violation, not ignored.
    assert!(parse_client_message(&json!({ "type": "hello", "version": 8, "extra": 1 })).is_err());
    // A non-integer version is rejected too.
    assert!(parse_client_message(&json!({ "type": "hello", "version": "8" })).is_err());
}

#[test]
fn client_message_rejects_a_cancel_with_a_malformed_target() {
    // `serverId` must be a lowercase UUIDv4, and the target takes no other keys.
    assert!(parse_client_message(&json!({
        "type": "cancel",
        "id": "1",
        "target": { "serverId": "not-a-uuid" },
    }))
    .is_err());
    assert!(parse_client_message(&json!({
        "type": "cancel",
        "id": "1",
        "target": { "serverId": "0f8fad5b-d9cb-469f-a165-70867728950e", "extra": 1 },
    }))
    .is_err());
}

#[test]
fn schema_validators_reject_non_objects() {
    assert!(parse_server_message(&json!("not an object")).is_err());
    assert!(parse_server_message(&json!(null)).is_err());
    assert!(parse_client_message(&json!([])).is_err());
}
