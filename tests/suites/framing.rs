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
