use pir::protocol::framing::FrameBuilder;
use pir::protocol::{encode_frame, FrameDecoder, FrameError, FrameOptions};

#[test]
fn fragments_and_coalesces_frames() {
    let mut decoder = FrameDecoder::new(FrameOptions::default()).expect("valid options");
    let mut wire = encode_frame(&[1, 2, 3]).expect("frame");
    wire.extend(encode_frame(&[]).expect("empty frame"));
    let mut result = Vec::new();
    for byte in wire {
        result.extend(decoder.push(&[byte]).expect("fragment is valid"));
    }
    decoder.end().expect("clean end");
    assert_eq!(result, vec![vec![1, 2, 3], vec![]]);
}

/// The single-allocation builder is an additive fast path: its bytes must be
/// indistinguishable from `encode_frame`'s, including header and large payload.
#[test]
fn frame_builder_is_byte_identical_to_encode_frame() {
    let mut large = vec![0xAB; 4 * 1024 * 1024];
    large[0] = 0;
    let last = large.len() - 1;
    large[last] = 0xFF;
    for payload in [Vec::new(), vec![0x42], (0..=255).collect(), large] {
        let mut builder = FrameBuilder::with_capacity(payload.len());
        builder.payload_mut().extend_from_slice(&payload);
        assert_eq!(builder.payload_len(), payload.len());
        let fast = builder.finish().expect("builder frame");
        let reference = encode_frame(&payload).expect("reference frame");
        assert_eq!(fast, reference);
    }
}

#[test]
fn oversized_length_is_terminal_failure() {
    let mut decoder = FrameDecoder::new(FrameOptions {
        max_frame_length: 2,
    })
    .expect("valid options");
    assert!(matches!(
        decoder.push(&[0, 0, 0, 3]),
        Err(FrameError::Limit { .. })
    ));
    assert_eq!(decoder.push(&[]), Err(FrameError::Failed));
}
