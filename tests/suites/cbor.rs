use pir::protocol::{decode_value, encode_cbor, CborError, CborOptions};

#[test]
fn rejects_duplicate_non_string_and_trailing_values() {
    let options = CborOptions::default();
    assert!(matches!(
        decode_value(&[0xa2, 0x61, b'a', 1, 0x61, b'a', 2], options),
        Err(CborError::Malformed(_))
    ));
    assert!(matches!(
        decode_value(&[0xa1, 1, 2], options),
        Err(CborError::Malformed(_))
    ));
    assert!(matches!(
        decode_value(&[0, 0], options),
        Err(CborError::Malformed(_))
    ));
}

#[test]
fn rejects_byte_strings_before_schema_validation() {
    // Major type 2 is not part of the v8 schema; accepting it would expand
    // every byte into a serde_json::Value before the schema check.
    let options = CborOptions::default();
    assert!(matches!(
        decode_value(&[0x41, 0x00], options),
        Err(CborError::Malformed(message)) if message.contains("byte string")
    ));
    assert!(matches!(
        decode_value(&[0xa1, 0x61, b'x', 0x41, 0x00], options),
        Err(CborError::Malformed(_))
    ));
}

#[test]
fn enforces_limits() {
    let options = CborOptions {
        max_container_length: 1,
        ..CborOptions::default()
    };
    assert!(encode_cbor(&serde_json::json!([1, 2]), options).is_err());
}

#[test]
fn golden_vectors_round_trip_without_wire_drift() {
    let vectors = [
        (serde_json::json!(null), vec![0xf6]),
        (serde_json::json!(false), vec![0xf4]),
        (serde_json::json!(true), vec![0xf5]),
        (serde_json::json!(0), vec![0x00]),
        (serde_json::json!(1), vec![0x01]),
        (serde_json::json!(-1), vec![0x20]),
        (serde_json::json!(23), vec![0x17]),
        (serde_json::json!(24), vec![0x18, 0x18]),
        (serde_json::json!(255), vec![0x18, 0xff]),
        (serde_json::json!(256), vec![0x19, 0x01, 0x00]),
        (serde_json::json!("hello"), b"\x65hello".to_vec()),
        (serde_json::json!("π"), vec![0x62, 0xcf, 0x80]),
        (serde_json::json!([]), vec![0x80]),
        (serde_json::json!({}), vec![0xa0]),
    ];

    for (value, expected) in vectors {
        assert_eq!(
            encode_cbor(&value, CborOptions::default()).unwrap(),
            expected
        );
        assert_eq!(
            decode_value(&expected, CborOptions::default()).unwrap(),
            value
        );
    }
}

#[test]
fn rejects_indefinite_and_short_float_encodings() {
    let options = CborOptions::default();
    assert!(matches!(
        decode_value(&[0x9f, 0x01, 0xff], options),
        Err(CborError::Malformed(_))
    ));
    assert!(matches!(
        decode_value(&[0xf9, 0x3c, 0x00], options),
        Err(CborError::Malformed(_))
    ));
}

#[test]
fn accepts_negative_zero_and_unicode_text() {
    let negative_zero = decode_value(
        &[0xfb, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        CborOptions::default(),
    )
    .expect("half-precision negative zero is valid CBOR");
    let number = negative_zero.as_f64().expect("decoded number");
    assert!(number == 0.0);
    assert!(number.is_sign_negative());

    let value = serde_json::json!("こんにちは 🌍 — café");
    let encoded = encode_cbor(&value, CborOptions::default()).expect("Unicode is valid text");
    assert_eq!(
        decode_value(&encoded, CborOptions::default()).unwrap(),
        value
    );
}

// ---------------------------------------------------------------------------
// Floats always encode as major-7 additional-27 (P0-1)
// ---------------------------------------------------------------------------

#[test]
fn floats_always_encode_as_eight_byte_doubles() {
    // (value, expected IEEE 754 f64 big-endian bytes after the 0xfb head).
    let vectors: [(serde_json::Value, [u8; 8]); 5] = [
        (serde_json::json!(0.5), 0.5f64.to_bits().to_be_bytes()),
        (serde_json::json!(0.0), 0.0f64.to_bits().to_be_bytes()),
        (serde_json::json!(3.0), 3.0f64.to_bits().to_be_bytes()),
        (serde_json::json!(0.1), 0.1f64.to_bits().to_be_bytes()),
        (serde_json::json!(-1.0), (-1.0f64).to_bits().to_be_bytes()),
    ];
    for (value, payload) in vectors {
        let encoded = encode_cbor(&value, CborOptions::default())
            .unwrap_or_else(|error| panic!("encode {value}: {error}"));
        // Byte-level assertion: 0xfb prefix + exactly 8 bytes. ciborium's
        // writer would shrink these to f16 (0xf9) or f32 (0xfa), which both
        // pir's decoder and upstream reject.
        assert_eq!(encoded.len(), 9, "encoding of {value}: {encoded:?}");
        assert_eq!(encoded[0], 0xfb, "encoding of {value}: {encoded:?}");
        assert_eq!(&encoded[1..], &payload, "encoding of {value}");
        // Round trip.
        assert_eq!(
            decode_value(&encoded, CborOptions::default()).unwrap(),
            value
        );
    }
}

#[test]
fn float_valued_costs_embed_eight_byte_doubles() {
    // Cost fields (schemas/types.rs) are f64; a whole-number cost like 3.0
    // must still encode as additional-27, never shrink to an integer-ish
    // smaller float width.
    let value = serde_json::json!({"cost": {"total": 3.0, "cacheRead": 0.5}});
    let encoded = encode_cbor(&value, CborOptions::default()).expect("valid value");
    assert!(encoded.contains(&0xfb), "missing 0xfb in {encoded:?}");
    assert_eq!(
        decode_value(&encoded, CborOptions::default()).unwrap(),
        value
    );
}

#[test]
fn rejects_f16_and_f32_float_encodings() {
    let options = CborOptions::default();
    // 0xf9 0x3c 0x00 is f16 1.0.
    assert!(matches!(
        decode_value(&[0xf9, 0x3c, 0x00], options),
        Err(CborError::Malformed(_))
    ));
    // 0xfa 0x3f 0x80 0x00 0x00 is f32 1.0.
    assert!(matches!(
        decode_value(&[0xfa, 0x3f, 0x80, 0x00, 0x00], options),
        Err(CborError::Malformed(_))
    ));
}

// ---------------------------------------------------------------------------
// Pre-decode wire validator depth limit (P1-2)
// ---------------------------------------------------------------------------

#[test]
fn hostile_nested_arrays_error_instead_of_stack_overflow() {
    let options = CborOptions::default();
    // 100_000 nested arrays around a single integer: 200 KB, well inside the
    // 16 MiB byte cap but far beyond the configured depth limit. The
    // pre-decode validator must bail with DepthLimit, not recurse to a stack
    // overflow.
    let depth = 100_000usize;
    let mut hostile = vec![0x81u8; depth]; // array(1) nesting
    hostile.push(0x00);
    let error = decode_value(&hostile, options).expect_err("must be rejected");
    assert!(
        matches!(error, CborError::DepthLimit(64)),
        "unexpected error: {error:?}"
    );

    // Same for nested maps.
    let mut hostile_map = Vec::with_capacity(3 * 100_000);
    for _ in 0..100_000 {
        hostile_map.extend_from_slice(&[0xa1, 0x61, b'k']); // {"k": ...}
    }
    hostile_map.push(0x00);
    let error = decode_value(&hostile_map, options).expect_err("must be rejected");
    assert!(matches!(error, CborError::DepthLimit(64)), "{error:?}");

    // A frame inside the depth limit still decodes.
    let mut ok = vec![0x81u8; 63];
    ok.push(0x00);
    assert!(decode_value(&ok, options).is_ok());
}
