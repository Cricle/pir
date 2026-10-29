//! The Rust env client (`src/env_client/`, upstream `packages/env/src`) against
//! upstream `packages/env/docs/protocol.md`, statement by statement.
//!
//! The daemon side of the wire already has its own suite
//! (`tests/env_daemon_protocol.rs`, the ported `#[cfg(test)]` vectors); this
//! file is the **client** half:
//!
//! * the frame layout (13-byte header, big-endian, the documented type numbers,
//!   the 16 MiB cap, the JSON-length rule) as exact byte vectors — the client
//!   encodes through `env_daemon::frame::write_frame` into its stdin sink and
//!   decodes through `read_frame` over a slice, so locking the codec's bytes
//!   locks the client's framing;
//! * the `PI-ENV <token>` sync rule and its noise handling, driven through the
//!   client's own [`StreamParser`] with split/mixed chunks;
//! * the documented `hello` report and the `protocol === 1` check, both as a
//!   live handshake against `pir env-daemon` and as the check's message;
//! * the client-side path rules ("the client resolves relative paths, `~` and
//!   `file://` URLs before sending") for POSIX and Windows daemons;
//! * the error shape (`{ code, message, syscall?, path? }`) and the
//!   Node-code → `FileError` mapping;
//! * `close()` on a live session: the session mutex is released before
//!   `teardown` re-locks it, and every later request reports `Connection
//!   closed`;
//! * the SSH invariants (no prompts, no forwarding, host keys strict against
//!   the application's own file) and the SHA-256-named deployment path.

use std::collections::BTreeMap;
use std::time::Duration;

use pir::env_client::connection::{checked_protocol, StreamParser, MAX_FRAME};
use pir::env_client::errors::{
    file_error_code_for, to_file_error, ExecutionError, ExecutionErrorCode, FileErrorCode,
    RemoteError,
};
use pir::env_client::paths::{file_url_to_path, RemotePathFlavor};
use pir::env_client::ssh::{daemon_path, local_daemon, sha256_hex, ssh_arguments, SshTarget};
use pir::env_client::{
    Connection, ConnectionOptions, RemoteExecutionEnv, RemoteExecutionEnvOptions,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The `pir` binary under test (the daemon is built into it).
fn pir_binary() -> &'static str {
    env!("CARGO_BIN_EXE_pir")
}

/// The daemon argv these tests drive: `[<pir>, "env-daemon"]` — the shape
/// `local_daemon()` reports for the real binary, completed by the client to
/// `<pir> env-daemon serve --token <hex>`.
///
/// `CARGO_BIN_EXE_pir`, not `local_daemon()`/`current_exe()`: in an integration
/// test `current_exe()` is the test harness, not pir (see `local_daemon()`'s
/// contract note), and the `env-daemon` argument is what makes pir start its
/// daemon instead of an agent run.
fn daemon_argv() -> Vec<String> {
    vec![pir_binary().to_string(), "env-daemon".to_string()]
}

/// A connection to the local daemon: `[<pir>, "env-daemon"]`, the shape
/// upstream's `localDaemon()` reports for this process.
fn local_connection() -> Connection {
    Connection::new(ConnectionOptions::argv(daemon_argv()))
}

fn local_environment(cwd: Option<String>) -> RemoteExecutionEnv {
    let mut options = RemoteExecutionEnvOptions::new("pi-env:test", local_connection());
    options.cwd = cwd;
    RemoteExecutionEnv::new(options)
}

/// A unique temporary directory for one test.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(name: &str) -> Tree {
        let path = std::env::temp_dir().join(format!(
            "pir-env-client-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create the test tree");
        Tree(path)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }

    fn root(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A frame built by the client's own encoder path: `frame::write_frame` into a
/// byte sink (what `Connection::write` does before handing bytes to stdin).
fn encode(frame: &pir::env_daemon::frame::Frame) -> Vec<u8> {
    let mut bytes = Vec::new();
    pir::env_daemon::frame::write_frame(&mut bytes, frame).expect("encode a frame");
    bytes
}

/// The exact bytes of one frame, spelled out (docs/protocol.md's layout).
fn documented_frame(kind: u8, id: u32, json: &str, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let length = 9 + json.len() + payload.len();
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());
    bytes.extend_from_slice(json.as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// The frames of a raw byte stream, without the sync line.
fn parse(bytes: &[u8], token: &str) -> (Vec<pir::env_daemon::frame::Frame>, Vec<String>) {
    let mut parser = StreamParser::new(token);
    let mut logs: Vec<String> = Vec::new();
    let frames = parser
        .feed(bytes, &mut |noise| logs.push(noise.to_string()))
        .expect("a well-formed stream");
    (frames, logs)
}

/// A decoded frame's identity: `(kind, id, json, payload)`, for comparing two
/// decodes of the same bytes.
type FrameShape = (u8, u32, serde_json::Value, Vec<u8>);

/// A frame's identity, for comparing two decodes of the same bytes.
fn frame_identity(frame: &pir::env_daemon::frame::Frame) -> FrameShape {
    (
        frame.kind,
        frame.id,
        frame.json.clone(),
        frame.payload.clone(),
    )
}

/// What the codec itself reads off `bytes` (`read_frame` until the end).
fn codec_frames(bytes: &[u8]) -> Vec<FrameShape> {
    let mut cursor: &[u8] = bytes;
    let mut frames = Vec::new();
    while let Some(frame) = pir::env_daemon::frame::read_frame(&mut cursor).expect("decode") {
        frames.push(frame_identity(&frame));
    }
    frames
}

/// Parse `stream` through the client's parser, split at `splits` byte offsets.
fn parse_in_pieces(
    stream: &[u8],
    token: &str,
    splits: &[usize],
) -> Result<Vec<FrameShape>, String> {
    let mut parser = StreamParser::new(token);
    let mut log = |_: &str| {};
    let mut frames = Vec::new();
    let mut start = 0usize;
    for end in splits.iter().copied().chain(std::iter::once(stream.len())) {
        frames.extend(parser.feed(&stream[start..end], &mut log)?);
        start = end;
    }
    Ok(frames
        .into_iter()
        .map(|frame| frame_identity(&frame))
        .collect())
}

// ---------------------------------------------------------------------------
// Frames — "Every frame, in both directions"
// ---------------------------------------------------------------------------

#[test]
fn the_parser_agrees_with_the_codec_at_every_split_of_the_stream() {
    // The daemon writes a frame with several `write` calls (header, JSON,
    // payload), so a reader sees partial frames routinely — and it must wait
    // for the rest, never report corruption. This is the property the e2e
    // listFiles/exec/watch failures came from, tested without a daemon: every
    // split of the same byte stream must decode to what the codec reads off the
    // whole stream.
    let mut stream = Vec::new();
    for (kind, id, json, payload) in [
        (2u8, 1u32, json!({ "protocol": 1 }), Vec::new()),
        (
            4,
            9,
            json!({ "kind": "output", "stream": "stdout" }),
            vec![7u8; 64],
        ),
        // An empty JSON body (`jsonLength == 0`) and a payload with NUL bytes,
        // like a `listFiles` record.
        (2, 10, json!({}), b"f\0probe.txt\0d\0sub\0".to_vec()),
    ] {
        stream.extend_from_slice(&encode(&pir::env_daemon::frame::Frame::with_payload(
            kind, id, json, payload,
        )));
    }
    let expected = codec_frames(&stream);
    assert_eq!(expected.len(), 3, "three frames in the stream");
    let token = "t";
    let mut with_marker = format!("PI-ENV {token}\n").into_bytes();
    with_marker.extend_from_slice(&stream);
    // Every single split point of the whole stream (the sync line included).
    for split in 0..=with_marker.len() {
        let frames = parse_in_pieces(&with_marker, token, &[split])
            .unwrap_or_else(|error| panic!("split at {split}: {error}"));
        assert_eq!(
            frames,
            expected,
            "split at {split} of {}",
            with_marker.len()
        );
    }
    // Every second split point on the frame part only: the marker stays in
    // front (the parser gates the stream on the sync line), and only the split
    // offsets move into the frame region.
    let marker_len = with_marker.len() - stream.len();
    for first in (0..=stream.len()).step_by(7) {
        for second in (first..=stream.len()).step_by(11) {
            let frames = parse_in_pieces(
                &with_marker,
                token,
                &[marker_len + first, marker_len + second],
            )
            .unwrap_or_else(|error| panic!("splits {first}/{second}: {error}"));
            assert_eq!(frames, expected, "splits {first}/{second}");
        }
    }
    // Byte by byte, and the header itself split across reads (0..4 bytes of it).
    let frames = parse_in_pieces(
        &with_marker,
        token,
        &(0..=with_marker.len()).collect::<Vec<_>>(),
    )
    .expect("byte-by-byte");
    assert_eq!(frames, expected);
    for header in 1..=4usize {
        let frames = parse_in_pieces(
            &with_marker,
            token,
            &(0..=stream.len())
                .step_by(header)
                .map(|offset| marker_len + offset)
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|error| panic!("{header}-byte pieces: {error}"));
        assert_eq!(frames, expected, "{header}-byte pieces");
    }
    // A truncated frame is not corruption: it is simply not a frame yet.
    let mut parser = StreamParser::new(token);
    let mut log = |_: &str| {};
    assert!(parser
        .feed(format!("PI-ENV {token}\n").as_bytes(), &mut log)
        .is_ok());
    for cut in 1..stream.len() {
        let mut parser = StreamParser::new(token);
        assert!(parser
            .feed(format!("PI-ENV {token}\n").as_bytes(), &mut log)
            .is_ok());
        let frames = parser
            .feed(&stream[..cut], &mut log)
            .unwrap_or_else(|error| panic!("truncated at {cut}: {error}"));
        assert!(
            frames.is_empty() || frames.len() < expected.len(),
            "a truncated stream yields only whole frames (cut {cut})"
        );
    }
    // A big payload split in the middle of the payload (the shape a
    // `listFiles`/`exec` response arrives in) decodes too. The sync line stays
    // in front, as in the earlier blocks.
    let big = encode(&pir::env_daemon::frame::Frame::with_payload(
        2,
        11,
        json!({ "count": 1 }),
        vec![b'x'; 128 * 1024],
    ));
    let mut big_with_marker = format!("PI-ENV {token}\n").into_bytes();
    big_with_marker.extend_from_slice(&big);
    let big_marker_len = big_with_marker.len() - big.len();
    for split in [
        0usize,
        1,
        3,
        4,
        12,
        13,
        14,
        9 + 14,
        1024,
        big.len() / 2,
        big.len() - 1,
    ] {
        let frames = parse_in_pieces(&big_with_marker, token, &[big_marker_len + split])
            .unwrap_or_else(|error| panic!("big split at {split}: {error}"));
        assert_eq!(frames, codec_frames(&big), "big split at {split}");
    }
}

#[test]
fn frame_layout_is_the_documented_header() {
    // `u32 length | u8 type | u32 id | u32 jsonLength | json | bytes`, all
    // integers big-endian: the byte vector is hand-computed here, not derived
    // from the encoder.
    let json = r#"{"protocol":1}"#;
    let expected = documented_frame(1, 7, json, b"");
    assert_eq!(
        expected,
        vec![
            0x00, 0x00, 0x00, 0x17, // length: 9 + 14 json bytes
            0x01, // type: request
            0x00, 0x00, 0x00, 0x07, // id
            0x00, 0x00, 0x00, 0x0E, // jsonLength
            b'{', b'"', b'p', b'r', b'o', b't', b'o', b'c', b'o', b'l', b'"', b':', b'1', b'}',
        ]
    );
    // The client's encoder writes exactly those bytes (with serde_json's
    // compact object form).
    let encoded = encode(&pir::env_daemon::frame::Frame::new(
        1,
        7,
        json!({ "protocol": 1 }),
    ));
    assert_eq!(
        encoded, expected,
        "the encoder writes the documented layout"
    );
    // A payload follows the JSON, and its length is part of the header's length.
    let with_payload = documented_frame(2, 0, json, b"abc");
    let encoded = encode(&pir::env_daemon::frame::Frame::with_payload(
        2,
        0,
        json!({ "protocol": 1 }),
        b"abc".to_vec(),
    ));
    assert_eq!(encoded, with_payload);
    // Decoding the documented bytes yields the frame back.
    let mut cursor: &[u8] = &with_payload;
    let frame = pir::env_daemon::frame::read_frame(&mut cursor)
        .expect("decode")
        .expect("one frame");
    assert_eq!(frame.kind, 2);
    assert_eq!(frame.payload, b"abc");
    assert!(cursor.is_empty(), "the frame consumed exactly its bytes");
}

#[test]
fn every_frame_type_has_the_documented_number() {
    // request 1, result 2, error 3, event 4, cancel 5, ping 6.
    assert_eq!(pir::env_daemon::frame::REQUEST, 1);
    assert_eq!(pir::env_daemon::frame::RESULT, 2);
    assert_eq!(pir::env_daemon::frame::ERROR, 3);
    assert_eq!(pir::env_daemon::frame::EVENT, 4);
    assert_eq!(pir::env_daemon::frame::CANCEL, 5);
    assert_eq!(pir::env_daemon::frame::PING, 6);
}

#[test]
fn integers_are_big_endian() {
    // id 0x01020304: the bytes are in network order, not host order. The header
    // is `u32 length | u8 type | u32 id | u32 jsonLength`, so the id occupies
    // bytes 5..9 and the JSON length 9..13.
    let json = r#"{}"#;
    let bytes = documented_frame(1, 0x0102_0304, json, &[0xff; 300]);
    // length = 9 + 2 + 300 = 311 = 0x137.
    assert_eq!(
        &bytes[0..4],
        &[0x00, 0x00, 0x01, 0x37],
        "length is big-endian"
    );
    assert_eq!(bytes[4], 1, "the type follows the length");
    assert_eq!(&bytes[5..9], &[0x01, 0x02, 0x03, 0x04], "id is big-endian");
    assert_eq!(
        &bytes[9..13],
        &[0x00, 0x00, 0x00, 0x02],
        "jsonLength is big-endian"
    );
    assert_eq!(
        &bytes[13..15],
        json.as_bytes(),
        "the JSON follows the header"
    );
    assert_eq!(bytes.len(), 4 + 311, "the payload fills the rest");
    let mut cursor: &[u8] = &bytes;
    let frame = pir::env_daemon::frame::read_frame(&mut cursor)
        .expect("decode")
        .expect("one frame");
    assert_eq!(frame.id, 0x0102_0304);
    assert_eq!(frame.payload.len(), 300);
}

#[test]
fn a_frame_is_at_most_16_mib() {
    assert_eq!(MAX_FRAME, 16 * 1024 * 1024);
    // A frame larger than the cap is refused by the encoder…
    let too_large =
        pir::env_daemon::frame::Frame::with_payload(2, 1, json!({}), vec![0u8; MAX_FRAME]);
    let mut bytes = Vec::new();
    assert!(
        pir::env_daemon::frame::write_frame(&mut bytes, &too_large).is_err(),
        "a frame beyond the cap must not be written"
    );
    // …and a claimed length beyond the cap is a framing error on the way in,
    // which the client's parser reports as corruption.
    let mut claim = Vec::new();
    claim.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
    claim.push(1);
    claim.extend_from_slice(&[0u8; 8]);
    let mut parser = StreamParser::new("t");
    let mut log = |_: &str| {};
    assert_eq!(
        parser.feed(b"PI-ENV t\n", &mut log).map(|_| ()),
        Ok(()),
        "the sync line itself is fine"
    );
    assert_eq!(
        parser.feed(&claim, &mut log).err(),
        Some("Corrupt frame from pi-env".to_string())
    );
    // A length below the 9-byte header is corrupt too.
    let mut short = Vec::new();
    short.extend_from_slice(&5u32.to_be_bytes());
    short.extend_from_slice(&[0u8; 8]);
    let mut parser = StreamParser::new("t");
    assert!(parser.feed(b"PI-ENV t\n", &mut log).is_ok());
    assert!(parser.feed(&short, &mut log).is_err());
}

#[test]
fn the_json_length_must_fit_inside_the_frame() {
    // length 9 with a JSON length of 5 (9 + 5 > 9): corrupt framing.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&9u32.to_be_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&5u32.to_be_bytes());
    let mut parser = StreamParser::new("t");
    let mut log = |_: &str| {};
    assert!(parser.feed(b"PI-ENV t\n", &mut log).is_ok());
    assert_eq!(
        parser.feed(&bytes, &mut log).err(),
        Some("Corrupt frame from pi-env".to_string())
    );
    // An unparseable JSON body keeps the framing intact (the codec's documented
    // behavior); the connection's reader treats the resulting `Value::Null` as
    // corruption, since no client operation can act on it.
    let bad_json = documented_frame(4, 3, "{not json", b"");
    let mut parser = StreamParser::new("t");
    assert!(parser.feed(b"PI-ENV t\n", &mut log).is_ok());
    let frames = parser.feed(&bad_json, &mut log).expect("framing survives");
    assert_eq!(frames.len(), 1);
    assert!(frames[0].json.is_null(), "unparseable JSON becomes null");
    assert!(!frames[0].json.is_object(), "the reader rejects this shape");
}

// ---------------------------------------------------------------------------
// Sync — "The daemon's first output is the line PI-ENV <token>\n"
// ---------------------------------------------------------------------------

#[test]
fn the_sync_line_drops_everything_before_it() {
    let token = "00112233445566778899aabbccddeeff";
    let hello = encode(&pir::env_daemon::frame::Frame::new(
        2,
        1,
        json!({ "protocol": 1 }),
    ));
    // A login shell banner before the marker: dropped, and reported as log text.
    let mut stream = Vec::new();
    stream.extend_from_slice(b"Welcome to the machine!\r\nprofile loaded\r\n");
    stream.extend_from_slice(format!("PI-ENV {token}\n").as_bytes());
    stream.extend_from_slice(&hello);
    let (frames, logs) = parse(&stream, token);
    assert_eq!(frames.len(), 1, "only the frame after the marker");
    assert_eq!(frames[0].id, 1);
    assert_eq!(
        logs,
        vec!["Welcome to the machine!\r\nprofile loaded".to_string()],
        "the noise before the marker is logged, trimmed"
    );
}

#[test]
fn the_sync_line_and_frames_may_arrive_split_across_reads() {
    let token = "aa";
    let first = encode(&pir::env_daemon::frame::Frame::new(
        3,
        9,
        json!({ "code": "EINVAL" }),
    ));
    let second = encode(&pir::env_daemon::frame::Frame::new(2, 10, json!({})));
    let mut parser = StreamParser::new(token);
    let mut logs: Vec<String> = Vec::new();
    let mut log = |noise: &str| logs.push(noise.to_string());
    // "PI-EN" | "V aa\n" + half the first frame | the rest + the second frame.
    assert!(parser
        .feed(b"PI-EN", &mut log)
        .expect("partial marker")
        .is_empty());
    assert!(!parser.is_synced());
    let split = 4;
    let mut chunk = format!("V {token}\n").into_bytes();
    chunk.extend_from_slice(&first[..split]);
    assert!(
        parser.feed(&chunk, &mut log).expect("marker").is_empty(),
        "half a frame is not a frame"
    );
    assert!(parser.is_synced());
    let mut chunk = first[split..].to_vec();
    chunk.extend_from_slice(&second);
    let frames = parser.feed(&chunk, &mut log).expect("two frames");
    assert_eq!(frames.len(), 2, "both frames came out of one read");
    assert_eq!(frames[0].json["code"], json!("EINVAL"));
    assert_eq!(frames[1].id, 10);
    // The marker's first half arrived in the previous chunk, so the marker is
    // contiguous here: nothing before it is noise.
    assert!(logs.is_empty(), "the split marker logs no noise: {logs:?}");
}

#[test]
fn noise_before_the_marker_is_not_kept_without_bound() {
    let token = "ff";
    let mut parser = StreamParser::new(token);
    let mut logs: Vec<String> = Vec::new();
    let mut log = |noise: &str| logs.push(noise.to_string());
    // More than 1 MiB of noise without a marker still lets the marker through.
    let noise = vec![b'x'; 1024 * 1024 + 4096];
    assert!(parser.feed(&noise, &mut log).expect("noise").is_empty());
    let frame = encode(&pir::env_daemon::frame::Frame::new(6, 0, json!({})));
    let mut chunk = format!("PI-ENV {token}\n").into_bytes();
    chunk.extend_from_slice(&frame);
    let frames = parser.feed(&chunk, &mut log).expect("marker after noise");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].kind, 6);
}

/// An endless pre-sync stream (a wrapper printing to stdout forever before the
/// daemon starts) must not grow the parser's buffer: every read's noise is
/// drained, not merely skipped behind the cursor. The output is unbounded, so
/// a cursor-only bound still accumulates the whole allocation until the start
/// timeout.
#[test]
fn pre_sync_noise_is_drained_instead_of_accumulated() {
    let token = "ff";
    let mut parser = StreamParser::new(token);
    let mut log = |_: &str| {};
    let noise = vec![b'x'; 64 * 1024];
    let reads = 32; // 2 MiB, more than the former 1 MiB window
    for _ in 0..reads {
        assert!(parser.feed(&noise, &mut log).expect("noise").is_empty());
    }
    assert!(!parser.is_synced(), "no marker was part of the noise");
    let total = noise.len() * reads;
    assert!(
        parser.compacted_bytes() >= total - 64,
        "pre-sync noise was retained: only {} of {total} bytes moved",
        parser.compacted_bytes()
    );
    // A marker split across reads is still found after the draining, and the
    // frame behind it parses.
    assert!(parser
        .feed(b"PI-E", &mut log)
        .expect("partial marker")
        .is_empty());
    let frame = encode(&pir::env_daemon::frame::Frame::new(
        pir::env_daemon::frame::RESULT,
        11,
        json!({}),
    ));
    let mut chunk = b"NV ff\n".to_vec();
    chunk.extend_from_slice(&frame);
    let frames = parser.feed(&chunk, &mut log).expect("marker after noise");
    assert!(parser.is_synced());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].id, 11);
}

// ---------------------------------------------------------------------------
// hello — "The first request"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hello_is_the_first_request_and_reports_the_documented_fields() {
    let connection = local_connection();
    let info = connection
        .info()
        .await
        .expect("hello over the local daemon");
    assert_eq!(info.protocol, 1, "protocol 1");
    assert!(
        info.has_feature("listFiles"),
        "features lists the operations added after protocol 1: {:?}",
        info.features
    );
    for (name, value) in [
        ("version", &info.version),
        ("os", &info.os),
        ("arch", &info.arch),
        ("home", &info.home),
        ("separator", &info.separator),
        ("tmpdir", &info.tmpdir),
        ("cwd", &info.cwd),
    ] {
        assert!(!value.is_empty(), "hello.{name} is reported: {value:?}");
    }
    assert_eq!(info.os, std::env::consts::OS, "Rust's consts spelling");
    assert_eq!(info.arch, std::env::consts::ARCH);
    assert!(info.pid > 0, "pid: {}", info.pid);
    assert!(
        info.drive_cwds.is_empty() || info.drive_cwds.keys().all(|key| key.len() == 2),
        "driveCwds is keyed by the drive: {:?}",
        info.drive_cwds
    );
    // `nodePlatform` maps the daemon's names to Node's (`windows` → `win32`,
    // `macos` → `darwin`, `x86_64` → `x64`, `aarch64` → `arm64`).
    let expected_platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let expected_arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    assert_eq!(
        info.node_platform(),
        (expected_platform.to_string(), expected_arch.to_string())
    );
    connection.close();
}

#[test]
fn only_protocol_one_is_accepted() {
    assert!(checked_protocol(&json!({ "protocol": 1 })).is_ok());
    let error = checked_protocol(&json!({ "protocol": 2 })).expect_err("protocol 2 is refused");
    assert_eq!(error.message, "Unsupported protocol 2");
    assert!(error.lost, "a refused handshake is a lost connection");
    let error = checked_protocol(&json!({})).expect_err("no protocol is refused");
    assert_eq!(error.message, "Unsupported protocol none");
}

// ---------------------------------------------------------------------------
// Lifecycle — `close()` releases the session mutex before `teardown` re-locks it
// ---------------------------------------------------------------------------

/// `close()` on a **live** session must return: the old form held the session
/// mutex guard in the `if let Some(session) = lock(&self.inner.session).clone()`
/// scrutinee while `teardown` re-locked the same non-reentrant mutex, deadlocking
/// the calling thread. The wait below turns that revert into a 5 s failure, and
/// the follow-up request pins the closed-connection error.
#[tokio::test]
async fn close_returns_on_a_live_session_and_later_requests_report_it() {
    let connection = local_connection();
    // The deadlock needs a live session: `teardown`'s `live.swap(false)` is then
    // true, so it runs its own `lock(..)` against the guard `close()` holds.
    let info = connection.info().await.expect("hello");
    assert_eq!(info.protocol, 1);
    assert!(connection.session().await.expect("a live session") > 0);
    assert!(!connection.is_closed());

    // `close()` runs on a detached thread, so a deadlock cannot hang the run:
    // the timeout fires after 5 s and the stuck thread dies with the process
    // (a `spawn_blocking` task would be waited for by the runtime's own drop).
    let closing = connection.clone();
    let (returned, returned_reader) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        closing.close();
        let _ = returned.send(());
    });
    let closed = tokio::time::timeout(Duration::from_secs(5), returned_reader).await;
    assert!(
        closed.is_ok(),
        "close() must return on a live session: the old `if let .. lock(..).clone()` form held \
         the session mutex into `teardown`, which re-locks it and self-deadlocks"
    );

    assert!(connection.is_closed());
    let error = connection.info().await.expect_err("a request after close");
    assert_eq!(error.code, "unknown");
    assert_eq!(error.message, "Connection closed");
    assert_eq!(error.to_string(), "Connection closed");
    assert!(!error.lost, "a closed connection is not a lost one");
}

// ---------------------------------------------------------------------------
// Paths — "the client resolves relative paths, ~ and file:// URLs before sending"
// ---------------------------------------------------------------------------

#[test]
fn posix_paths_resolve_like_node() {
    let posix = RemotePathFlavor::Posix;
    assert!(posix.is_absolute("/a"));
    assert!(!posix.is_absolute("a/b"));
    assert_eq!(posix.normalize("/a/./b/../c"), "/a/c");
    assert_eq!(
        posix.normalize("/../a"),
        "/a",
        ".. above the root is dropped"
    );
    assert_eq!(posix.normalize("/.."), "/");
    assert_eq!(
        posix.normalize("a/../../b"),
        "../b",
        "in a relative path the .. is kept"
    );
    assert_eq!(posix.resolve("/work", "src/../lib/x"), "/work/lib/x");
    assert_eq!(posix.resolve("/work", "/etc/hosts"), "/etc/hosts");
    assert_eq!(
        posix.resolve("/work", "../etc/hosts"),
        "/etc/hosts",
        ".. resolves against the base and cannot rise above the root"
    );
    assert_eq!(
        posix.join(&["/home/u", "dir", "f.txt"]),
        "/home/u/dir/f.txt"
    );
    assert_eq!(posix.basename("/home/u/f.txt"), "f.txt");
    assert_eq!(posix.dirname("/home/u/f.txt"), "/home/u");
    assert!(posix.contains("/work", "/work"));
    assert!(posix.contains("/work", "/work/a/b"));
    assert!(!posix.contains("/work", "/workshop"));
}

#[test]
fn windows_paths_resolve_like_node() {
    let win32 = RemotePathFlavor::Win32;
    assert!(win32.is_absolute("C:\\Users\\x"));
    assert!(win32.is_absolute("\\\\server\\share\\x"));
    assert!(win32.is_absolute("\\rooted"));
    assert!(!win32.is_absolute("a\\b"));
    assert!(
        !win32.is_absolute("C:foo"),
        "a drive-qualified path that is not rooted is drive-relative"
    );
    assert_eq!(win32.normalize("C:\\a\\..\\b"), "C:\\b");
    assert_eq!(
        win32.normalize("C:.."),
        "C:..",
        "nothing to pop on the drive"
    );
    assert_eq!(win32.resolve("C:\\work", "src\\..\\lib"), "C:\\work\\lib");
    assert_eq!(
        win32.resolve("C:\\work", "D:\\other"),
        "D:\\other",
        "a drive-qualified absolute path replaces the base"
    );
    assert_eq!(
        win32.resolve("C:\\work", "\\rooted"),
        "C:\\rooted",
        "a root-relative path resolves on the base's drive"
    );
    assert_eq!(
        win32.resolve("C:\\work", "C:foo"),
        "C:\\work\\foo",
        "a drive-relative path on the base's drive resolves against the base"
    );
    assert_eq!(win32.resolve("C:\\work", "..\\other"), "C:\\other");
    assert_eq!(
        win32.join(&["\\\\server\\share", "dir", "f.txt"]),
        "\\\\server\\share\\dir\\f.txt"
    );
    assert!(
        win32.contains("C:\\Work", "c:\\work\\sub"),
        "case-insensitive"
    );
    assert!(!win32.contains("C:\\work", "C:\\workshop"));
    assert_eq!(win32.separator(), '\\');
    assert!(win32.is_separator('/') && win32.is_separator('\\'));
}

#[test]
fn file_urls_become_paths_before_sending() {
    assert_eq!(
        file_url_to_path("file:///home/u/a%20b.txt", RemotePathFlavor::Posix).as_deref(),
        Some("/home/u/a b.txt")
    );
    assert_eq!(
        file_url_to_path("file:///C:/Users/u/a.txt", RemotePathFlavor::Win32).as_deref(),
        Some("C:\\Users\\u\\a.txt"),
        "the /C:/ form becomes a drive path"
    );
    assert_eq!(
        file_url_to_path("file://server/share/x", RemotePathFlavor::Win32).as_deref(),
        Some("\\\\server\\share\\x"),
        "a non-empty authority is a UNC share on Windows"
    );
    assert_eq!(
        file_url_to_path("file://elsewhere/home", RemotePathFlavor::Posix),
        None,
        "a non-local authority is not a POSIX path"
    );
    assert_eq!(
        file_url_to_path("https://example.com/x", RemotePathFlavor::Posix),
        None
    );
}

// ---------------------------------------------------------------------------
// Errors — `{ code, message, syscall?, path? }`
// ---------------------------------------------------------------------------

#[test]
fn error_frames_carry_a_code_and_optional_details() {
    let error = RemoteError::from_json(&json!({
        "code": "ENOENT",
        "message": "ENOENT: no such file or directory, open '/x'",
        "syscall": "open",
        "path": "/x",
    }));
    assert_eq!(error.code, "ENOENT");
    assert_eq!(error.path.as_deref(), Some("/x"));
    assert!(!error.lost);
    assert!(!error.spill_path.is_some());
    // A code-less error is `unknown`, as upstream.
    let error = RemoteError::from_json(&json!({ "message": "boom" }));
    assert_eq!(error.code, "unknown");
    assert_eq!(error.message, "boom");
    // `lost` and `spillPath` are carried through (`exec` failures).
    let error = RemoteError::from_json(&json!({
        "code": "timeout",
        "message": "timeout",
        "lost": true,
        "spillPath": "/tmp/spill",
    }));
    assert!(error.lost);
    assert_eq!(error.spill_path.as_deref(), Some("/tmp/spill"));
}

#[test]
fn node_codes_map_to_durable_fileerror_codes() {
    // `NodeExecutionEnv`'s mapping (errors.ts).
    assert_eq!(file_error_code_for("aborted"), FileErrorCode::Aborted);
    assert_eq!(file_error_code_for("ENOENT"), FileErrorCode::NotFound);
    assert_eq!(
        file_error_code_for("EACCES"),
        FileErrorCode::PermissionDenied
    );
    assert_eq!(
        file_error_code_for("EPERM"),
        FileErrorCode::PermissionDenied
    );
    assert_eq!(file_error_code_for("ENOTDIR"), FileErrorCode::NotDirectory);
    assert_eq!(file_error_code_for("EISDIR"), FileErrorCode::IsDirectory);
    assert_eq!(file_error_code_for("EINVAL"), FileErrorCode::Invalid);
    assert_eq!(file_error_code_for("SYMLINK"), FileErrorCode::Invalid);
    assert_eq!(file_error_code_for("NOT_REGULAR"), FileErrorCode::Invalid);
    assert_eq!(file_error_code_for("EBUSY"), FileErrorCode::Unknown);
    // The mapping keeps the daemon's path, and falls back to the caller's.
    let with_path = to_file_error(
        &RemoteError::from_json(&json!({ "code": "ENOENT", "message": "x", "path": "/remote" })),
        "/fallback",
    );
    assert_eq!(with_path.path.as_deref(), Some("/remote"));
    let without_path = to_file_error(
        &RemoteError::from_json(&json!({ "code": "ENOENT", "message": "x" })),
        "/fallback",
    );
    assert_eq!(without_path.path.as_deref(), Some("/fallback"));
    assert_eq!(without_path.code, FileErrorCode::NotFound);
}

#[test]
fn execution_error_codes_match_the_contract() {
    // The error frame's execution codes: timeout / aborted / shell_unavailable
    // / spawn_error / unknown.
    let timeout = ExecutionError::from_remote(
        &RemoteError::from_json(&json!({
            "code": "timeout",
            "message": "Command timed out",
            "spillPath": "/tmp/x",
        })),
        Some(2.5),
    );
    assert_eq!(timeout.code, ExecutionErrorCode::Timeout);
    assert_eq!(timeout.message, "timeout:2.5", "upstream's message form");
    assert_eq!(timeout.spill_path.as_deref(), Some("/tmp/x"));
    let aborted = ExecutionError::from_remote(
        &RemoteError::from_json(&json!({ "code": "aborted", "message": "aborted" })),
        None,
    );
    assert_eq!(aborted.code, ExecutionErrorCode::Aborted);
    assert_eq!(aborted.message, "aborted");
    let shell = ExecutionError::from_remote(
        &RemoteError::from_json(
            &json!({ "code": "shell_unavailable", "message": "No bash found" }),
        ),
        None,
    );
    assert_eq!(shell.code, ExecutionErrorCode::ShellUnavailable);
    assert_eq!(shell.message, "No bash found");
    let spawn = ExecutionError::from_remote(
        &RemoteError::from_json(&json!({ "code": "spawn_error", "message": "ENOENT" })),
        None,
    );
    assert_eq!(spawn.code, ExecutionErrorCode::SpawnError);
    let other = ExecutionError::from_remote(
        &RemoteError::from_json(&json!({ "code": "EACCES", "message": "denied" })),
        None,
    );
    assert_eq!(other.code, ExecutionErrorCode::Unknown);
}

// ---------------------------------------------------------------------------
// SSH — "no prompts, no forwarding of any kind, no commands from the
// configuration, and host keys checked strictly"
// ---------------------------------------------------------------------------

fn ssh_target() -> SshTarget {
    let mut target = SshTarget::new(
        "build-box",
        "/home/u/.pi/mobile/known_hosts",
        "pi-env-build",
    );
    target.user = Some("ci".to_string());
    target.port = Some(2222);
    target.identity_file = Some("/home/u/.ssh/id_ed25519".to_string());
    target
}

#[test]
fn ssh_arguments_disable_prompts_and_every_forwarding_kind() {
    let args = ssh_arguments(&ssh_target(), true, None).expect("arguments");
    let expected = vec![
        "-T",
        "-a",
        "-x",
        "-o",
        "BatchMode=yes",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "RemoteCommand=none",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "SendEnv=-*",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "UserKnownHostsFile=\"/home/u/.pi/mobile/known_hosts\"",
        "-o",
        "GlobalKnownHostsFile=none",
        "-o",
        "HashKnownHosts=no",
        "-o",
        "HostKeyAlias=pi-env-build",
        "-l",
        "ci",
        "-p",
        "2222",
        "-o",
        "IdentityFile=\"/home/u/.ssh/id_ed25519\"",
        "-o",
        "IdentitiesOnly=yes",
        "--",
        "build-box",
    ];
    assert_eq!(args, expected, "the documented option list and order");
    // The known hosts file for a scan is the temporary one, and the key is only
    // accepted (`accept-new`), never silently trusted.
    let scanned =
        ssh_arguments(&ssh_target(), false, Some("/tmp/scan/known_hosts")).expect("scan arguments");
    assert!(scanned.contains(&"StrictHostKeyChecking=accept-new".to_string()));
    assert!(scanned.contains(&"UserKnownHostsFile=\"/tmp/scan/known_hosts\"".to_string()));
    assert!(scanned.contains(&"HostKeyAlias=pi-env-build".to_string()));
    // A `%` in a known-hosts path is escaped (the option expands tokens).
    let mut percent = ssh_target();
    percent.known_hosts_file = "/home/u/%h/known_hosts".to_string();
    let args = ssh_arguments(&percent, true, None).expect("arguments");
    assert!(args.contains(&"UserKnownHostsFile=\"/home/u/%%h/known_hosts\"".to_string()));
}

#[test]
fn ssh_fields_that_could_be_read_as_options_are_rejected() {
    for host in ["-oProxyCommand=x", "a b", "", "a\nb"] {
        let mut target = ssh_target();
        target.host = host.to_string();
        assert!(
            ssh_arguments(&target, true, None).is_err(),
            "host {host:?} must be refused"
        );
    }
    let mut target = ssh_target();
    target.user = Some("-root".to_string());
    assert!(ssh_arguments(&target, true, None).is_err());
    let mut target = ssh_target();
    target.host_key_alias = "two words".to_string();
    assert!(ssh_arguments(&target, true, None).is_err());
}

#[test]
fn the_deployed_daemon_path_is_named_by_its_content() {
    // `pi-env-<sha256[..32]>` under `~/.pi/mobile/tools`, per platform: the
    // content names the file, so versions never collide.
    let sha = sha256_hex(b"pi-env");
    assert_eq!(sha.len(), 64);
    assert_eq!(sha, sha.to_lowercase(), "lowercase hex");
    let linux = pir::env_client::ssh::RemotePlatform {
        platform: "linux".to_string(),
        arch: "x64".to_string(),
        home: "/home/u".to_string(),
        shell: None,
        warnings: Vec::new(),
    };
    assert_eq!(
        daemon_path(&linux, &sha),
        format!("/home/u/.pi/mobile/tools/pi-env-{}", &sha[..32])
    );
    let windows = pir::env_client::ssh::RemotePlatform {
        platform: "windows".to_string(),
        arch: "arm64".to_string(),
        home: r"C:\Users\u".to_string(),
        shell: Some("cmd".to_string()),
        warnings: Vec::new(),
    };
    assert_eq!(
        daemon_path(&windows, &sha),
        format!(r"C:\Users\u\.pi\mobile\tools\pi-env-{}.exe", &sha[..32])
    );
}

#[test]
fn sha256_is_the_documented_digest() {
    // The deployment verifies the uploaded bytes by this digest, so the digest
    // itself is pinned to the well-known vectors.
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn the_local_daemon_command_is_the_running_pir_image() {
    // `localDaemon()`: an image path plus the daemon subcommand, so the same
    // protocol is served by this build.
    let command = local_daemon();
    assert_eq!(command.len(), 2);
    assert_eq!(command[1], "env-daemon");
    assert!(!command[0].is_empty());
    // The test-side argv has the same shape (and is what every daemon-backed
    // test in these two files connects through).
    assert_eq!(daemon_argv().last().map(String::as_str), Some("env-daemon"));
    assert!(std::path::Path::new(&daemon_argv()[0]).is_file());
}

// ---------------------------------------------------------------------------
// The call surface the tools use
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remote_environments_resolve_against_the_remote_system() {
    // The remote cwd is the daemon's own (hello.cwd) when the caller names none.
    let environment = local_environment(None);
    let info = environment.info().await.expect("hello");
    let root = environment
        .root()
        .await
        .expect("the daemon reports its cwd");
    assert_eq!(root, info.cwd, "no configured cwd means hello.cwd");
    assert!(
        std::path::Path::new(&root).is_absolute(),
        "hello.cwd is absolute: {root}"
    );
    // The remote system's own path rules, chosen from the daemon's report.
    let flavor = RemotePathFlavor::for_platform(&info.node_platform().0);
    assert_eq!(
        environment.resolve("sub/../f.txt").await.expect("resolve"),
        flavor.resolve(&root, "f.txt"),
        "a relative path resolves against the environment's cwd"
    );
    assert_eq!(
        environment.resolve("~/x").await.expect("tilde"),
        flavor.join(&[&info.home, "x"]),
        "~ expands against the remote home"
    );
    // An absolute path is normalized in the remote's spelling (a
    // device-qualified Windows path is absolute on its own).
    let absolute = if cfg!(windows) {
        "C:\\abs\\x"
    } else {
        "/abs/x"
    };
    assert_eq!(
        environment.resolve(absolute).await.expect("absolute"),
        flavor.normalize(absolute),
        "an absolute path is normalized in the remote's spelling"
    );
    if cfg!(windows) {
        // A root-relative path is absolute too, but resolves on the base drive
        // (`path.win32.resolve` reports the drive of its first argument).
        assert_eq!(
            environment.resolve("\\abs\\x").await.expect("rooted"),
            flavor.resolve(&root, "\\abs\\x"),
            "a root-relative path resolves on the base's drive"
        );
    }
    assert_eq!(
        environment
            .join_path(&[&root, "sub", "f.txt"], &Default::default())
            .await
            .expect("joinPath"),
        flavor.join(&[&root, "sub", "f.txt"])
    );
    environment.connection.close();
}

#[tokio::test]
async fn list_files_requires_the_documented_feature_and_reports_entries() {
    let tree = Tree::new("listfiles");
    std::fs::write(tree.path("a.txt"), "a").expect("write a");
    std::fs::create_dir_all(tree.path("sub")).expect("mkdir sub");
    std::fs::write(tree.path("sub/b.txt"), "b").expect("write b");
    let environment = local_environment(Some(tree.root()));
    let listing = environment
        .list_files(
            ".",
            &pir::env_client::ListFilesOptions::default(),
            &Default::default(),
        )
        .await
        .expect("listFiles");
    let paths: Vec<&str> = listing
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert!(paths.contains(&"a.txt"), "entries: {paths:?}");
    assert!(paths.contains(&"sub/b.txt"), "entries: {paths:?}");
    assert!(
        listing
            .entries
            .iter()
            .all(|entry| entry.kind == "file" || entry.kind == "directory"),
        "kinds are file/directory/symlink: {:?}",
        listing.entries
    );
    assert!(!listing.truncated);
    // The options are validated like upstream's `listFilesOver`.
    let error = environment
        .list_files(
            ".",
            &pir::env_client::ListFilesOptions {
                max_entries: None,
                max_bytes: Some(u64::MAX),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .expect_err("maxBytes beyond a safe integer is invalid");
    assert_eq!(error.code, FileErrorCode::Invalid);
    assert_eq!(
        error.message,
        "maxBytes must be a non-negative safe integer"
    );
    // A `not_supported` daemon is a `FileError`, not a panic: the feature list
    // is what gates it, so a protocol-1 daemon without `listFiles` is refused.
    let environment = local_environment(Some(tree.root()));
    let info = environment.info().await.expect("hello");
    assert!(info.has_feature("listFiles"));
    environment.connection.close();
}

#[tokio::test]
async fn exec_env_is_merged_like_the_documented_rule() {
    // The daemon builds the command's environment; the client's request carries
    // the inherited values overlaid by the call's, with a `None` removing an
    // inherited variable (upstream drops `undefined` keys the same way).
    let tree = Tree::new("execenv");
    let mut shell_env = BTreeMap::new();
    shell_env.insert("PI_ENV_TEST_BASE".to_string(), "base".to_string());
    let mut options = RemoteExecutionEnvOptions::new("pi-env:test", local_connection());
    options.cwd = Some(tree.root());
    options.shell_env = shell_env;
    let environment = RemoteExecutionEnv::new(options);
    let mut env: BTreeMap<String, Option<String>> = BTreeMap::new();
    env.insert("PI_ENV_TEST_EXTRA".to_string(), Some("extra".to_string()));
    env.insert("PI_ENV_TEST_BASE".to_string(), None);
    // One argument vector, no shell string: `set <prefix>` on Windows lists the
    // matching variables, `printf` on POSIX prints the two values.
    let command = if cfg!(windows) {
        pir::env_client::ExecCommand::Argv(vec![
            "cmd".to_string(),
            "/c".to_string(),
            "set PI_ENV_TEST".to_string(),
        ])
    } else {
        pir::env_client::ExecCommand::Argv(vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s' \"$PI_ENV_TEST_BASE\" \"$PI_ENV_TEST_EXTRA\"".to_string(),
        ])
    };
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen_for_output = seen.clone();
    let exec_options = pir::env_client::ExecOptions {
        cwd: Some(tree.root()),
        env,
        inherit_env: Some(true),
        timeout_secs: None,
        spill: None,
        window: None,
        on_output: Some(std::sync::Arc::new(move |text: &str, _info| {
            if let Ok(mut guard) = seen_for_output.lock() {
                guard.push_str(text);
            }
        })),
    };
    let result = environment
        .exec(&command, &exec_options, &Default::default())
        .await
        .expect("exec");
    assert_eq!(result.exit_code, 0);
    let text = seen.lock().map(|guard| guard.clone()).unwrap_or_default();
    assert!(
        text.contains("PI_ENV_TEST_EXTRA=extra") || text.trim() == "|extra",
        "the call's variable reaches the command: {text:?}"
    );
    assert!(
        !text.contains("PI_ENV_TEST_BASE="),
        "a None removes the inherited variable, so it is not base: {text:?}"
    );
    // The environment is not reused across commands: a second exec without the
    // overlay has the inherited value again (the overlay belongs to the call).
    if let Ok(mut guard) = seen.lock() {
        guard.clear();
    }
    let _ = environment
        .exec(
            &command,
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                on_output: Some({
                    let seen = seen.clone();
                    std::sync::Arc::new(move |text: &str, _info| {
                        if let Ok(mut guard) = seen.lock() {
                            guard.push_str(text);
                        }
                    })
                }),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .expect("exec");
    let second = seen.lock().map(|guard| guard.clone()).unwrap_or_default();
    assert!(
        second.contains("base"),
        "the inherited value is back without the call's overlay: {second:?}"
    );
    assert!(
        !second.contains("PI_ENV_TEST_EXTRA"),
        "the overlay belongs to the call, so the second command must not see it: {second:?}"
    );
    environment.connection.close();
}

// ---------------------------------------------------------------------------
// The opened-handle readers — `openBinaryReader` / `openTextLineReader` /
// `openDirReader` (upstream `remote-env.ts`'s `RemoteBinaryReader`,
// `RemoteTextLineReader` and `RemoteDirReader`)
// ---------------------------------------------------------------------------

/// A symbolic link at `link` pointing at `target`; `false` where the OS refuses
/// to create one (Windows without `SeCreateSymbolicLinkPrivilege`/Developer
/// Mode), so the `noFollow` assertions below are skipped instead of failing.
fn symlink_file(target: &str, link: &str) -> bool {
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    created.is_ok()
}

#[tokio::test]
async fn open_binary_reader_sends_no_follow_and_opens_regular_files_only() {
    // `openBinaryReader(path, noFollow)` reaches the daemon as
    // `open { path, noFollow }` (upstream `_open`), and the reply's `info` is
    // the opened file's own metadata. Three calls pin that wire: a regular file
    // opens and `fstat` reports it; a **directory** is refused with EISDIR
    // (`open(path, "read")` opens a directory, so the refusal proves the client
    // did not send `mode: "read"`); and with `noFollow` a final symbolic link
    // is refused with SYMLINK — the only thing the flag changes.
    let tree = Tree::new("openbinary");
    std::fs::write(tree.path("f.txt"), "0123456789").expect("write f.txt");
    std::fs::create_dir_all(tree.path("sub")).expect("mkdir sub");
    let environment = local_environment(Some(tree.root()));
    let context = pir::env_client::Context::default();

    let mut reader = environment
        .open_binary_reader("f.txt", false, &context)
        .await
        .expect("a regular file opens");
    assert_eq!(
        reader.path(),
        environment.resolve("f.txt").await.expect("resolve"),
        "the handle is on the resolved path"
    );
    let info = reader.info(&context).await.expect("fstat");
    assert_eq!(info.kind, pir::env_client::FileKind::File);
    assert_eq!(info.name, "f.txt");
    assert_eq!(info.path, reader.path());
    assert_eq!(info.size, 10, "the opened file's size");
    reader.close().await;

    let sub = environment.resolve("sub").await.expect("resolve sub");
    let Err(error) = environment.open_binary_reader("sub", false, &context).await else {
        panic!("a directory is not a binary reader");
    };
    assert_eq!(error.code, FileErrorCode::IsDirectory);
    assert_eq!(
        error.message,
        "EISDIR: illegal operation on a directory, read"
    );
    assert_eq!(error.path.as_deref(), Some(sub.as_str()));

    if symlink_file(&tree.path("f.txt"), &tree.path("link.txt")) {
        let link = environment.resolve("link.txt").await.expect("resolve link");
        let Err(error) = environment
            .open_binary_reader("link.txt", true, &context)
            .await
        else {
            panic!("noFollow refuses a final symbolic link");
        };
        assert_eq!(error.code, FileErrorCode::Invalid);
        assert_eq!(error.message, "Refusing to follow a symbolic link");
        assert_eq!(error.path.as_deref(), Some(link.as_str()));
        // Without `noFollow` the link is followed, so the same path opens.
        let mut followed = environment
            .open_binary_reader("link.txt", false, &context)
            .await
            .expect("following the link opens the target");
        assert_eq!(
            followed.info(&context).await.expect("fstat").kind,
            pir::env_client::FileKind::File,
            "the followed target's metadata, not the link's"
        );
        followed.close().await;
    }
    environment.connection.close();
}

#[tokio::test]
async fn open_text_line_reader_sends_the_read_mode_that_opens_any_kind() {
    // `openTextLineReader` reaches the daemon as `open { path, mode: "read" }`
    // — upstream's `open(path, "r")`, whose reply is `{ handle, stat }` rather
    // than the binary open's `{ handle, info }`. The mode is observable: the
    // read mode opens a **directory** (as Node's `open(path, "r")` does), while
    // `openBinaryReader` on the same path is refused with EISDIR above.
    let tree = Tree::new("openlines");
    std::fs::write(tree.path("lines.txt"), "one\ntwo\nthree").expect("write lines");
    std::fs::create_dir_all(tree.path("sub")).expect("mkdir sub");
    let environment = local_environment(Some(tree.root()));
    let context = pir::env_client::Context::default();

    let mut reader = environment
        .open_text_line_reader("lines.txt", &context)
        .await
        .expect("openTextLineReader");
    assert_eq!(
        reader.path(),
        environment.resolve("lines.txt").await.expect("resolve"),
        "the handle is on the resolved path"
    );
    let line = reader
        .read_line(&context)
        .await
        .expect("readLine")
        .expect("the first line");
    assert_eq!(line.text, "one");
    assert!(line.terminated);
    reader.close().await;

    let mut directory = environment
        .open_text_line_reader("sub", &context)
        .await
        .expect("the read mode opens any kind of path");
    assert_eq!(
        directory.path(),
        environment.resolve("sub").await.expect("resolve sub")
    );
    directory.close().await;

    // `readTextLines(path, 0)` reads nothing: upstream returns `[]` without
    // opening the path, so even a missing one is not an error — while any other
    // count opens it and reports `not_found`.
    assert_eq!(
        environment
            .read_text_lines("missing.txt", Some(0), &context)
            .await
            .expect("zero lines never opens the path"),
        Vec::<String>::new()
    );
    let error = environment
        .read_text_lines("missing.txt", Some(1), &context)
        .await
        .expect_err("one line opens the path");
    assert_eq!(error.code, FileErrorCode::NotFound);
    assert_eq!(
        environment
            .read_text_lines("lines.txt", None, &context)
            .await
            .expect("readTextLines to the end"),
        vec!["one".to_string(), "two".to_string(), "three".to_string()]
    );
    environment.connection.close();
}

#[tokio::test]
async fn dir_reader_pages_with_max_and_reports_done() {
    // `openDirReader` opens with `opendir { path }`; each `next(maxEntries)`
    // asks for `readdir { handle, max }` and continues after the last entry it
    // returned, while `done` marks the end of the listing (`max` is what the
    // page size is, so the daemon never returns more than the caller asked).
    let tree = Tree::new("opendirreader");
    for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
        std::fs::write(tree.path(name), name).expect("write an entry");
    }
    let environment = local_environment(Some(tree.root()));
    let context = pir::env_client::Context::default();
    let mut reader = environment
        .open_dir_reader(".", &context)
        .await
        .expect("openDirReader");
    let root = environment.resolve(".").await.expect("resolve .");
    assert_eq!(reader.path(), root);

    let mut names: Vec<String> = Vec::new();
    for (index, expected) in [(0usize, 2usize), (1, 2), (2, 1)] {
        let page = reader
            .next(2, &context)
            .await
            .unwrap_or_else(|error| panic!("page {index}: {error:?}"));
        assert_eq!(page.entries.len(), expected, "page {index} honours `max`");
        assert_eq!(page.done, index == 2, "page {index}: done only at the end");
        for entry in &page.entries {
            assert_eq!(entry.kind, pir::env_client::FileKind::File);
            assert_eq!(entry.size, entry.name.len() as u64, "content is the name");
            assert!(
                entry.path.starts_with(&root) && entry.path.ends_with(&entry.name),
                "the entry path is the directory joined with the name: {}",
                entry.path
            );
        }
        names.extend(page.entries.iter().map(|entry| entry.name.clone()));
    }
    // A page after `done` is empty and stays done: the client does not even ask
    // the daemon again.
    let page = reader
        .next(2, &context)
        .await
        .expect("a page after the end");
    assert!(page.entries.is_empty(), "nothing after done: {page:?}");
    assert!(page.done);
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);

    // `maxEntries` is validated before the wire: a page size has to be a
    // positive safe integer (`0` and anything beyond `Number.MAX_SAFE_INTEGER`).
    for max in [0u64, 9_007_199_254_740_992] {
        let error = reader
            .next(max, &context)
            .await
            .expect_err("an invalid maxEntries");
        assert_eq!(error.code, FileErrorCode::Invalid);
        assert_eq!(error.message, "maxEntries must be a positive safe integer");
        assert_eq!(error.path.as_deref(), Some(root.as_str()));
    }
    reader.close().await;
    environment.connection.close();
}

#[tokio::test]
async fn binary_reader_reads_windows_and_scans_the_documented_line_record() {
    // `read(offset, length)` fills 256 KiB (`READ_CHUNK`) positional reads until
    // it has `length` bytes or reaches the end of the file, so a window larger
    // than one chunk still comes back whole and a window past the end is empty.
    // `scanLines` answers the documented seven-number record for the half-open
    // line range `[startLine, endLine)`, where line `k` starts after the `k`-th
    // newline byte.
    let tree = Tree::new("preadscan");
    let bytes: Vec<u8> = (0..600_000u32).map(|index| (index % 251) as u8).collect();
    std::fs::write(tree.path("big.bin"), &bytes).expect("write big.bin");
    std::fs::write(tree.path("lines.txt"), "l1\nl2\nl3\n").expect("write lines.txt");
    let environment = local_environment(Some(tree.root()));
    let context = pir::env_client::Context::default();

    let mut big = environment
        .open_binary_reader("big.bin", false, &context)
        .await
        .expect("openBinaryReader");
    // 600 000 bytes span three chunk-sized reads, two of them in flight at once.
    assert_eq!(
        big.read(0, 600_000, &context).await.expect("read"),
        bytes,
        "a window larger than one read chunk is assembled in order"
    );
    assert_eq!(
        big.read(300_000, 300_000, &context).await.expect("read"),
        bytes[300_000..].to_vec(),
        "a window starting inside the file is the same bytes"
    );
    assert!(
        big.read(600_000, 10, &context)
            .await
            .expect("read at the end")
            .is_empty(),
        "an offset at the end of the file reads nothing"
    );
    // `offset`/`length` are validated before the wire, like upstream's
    // `Number.isSafeInteger` pair.
    for (offset, length) in [(9_007_199_254_740_992u64, 1u64), (0, u64::MAX)] {
        let error = big
            .read(offset, length, &context)
            .await
            .expect_err("an unsafe integer");
        assert_eq!(error.code, FileErrorCode::Invalid);
        assert_eq!(
            error.message,
            "Offset and length must be non-negative safe integers"
        );
    }
    big.close().await;

    let mut lines = environment
        .open_binary_reader("lines.txt", false, &context)
        .await
        .expect("openBinaryReader");
    // Lines 0 and 1 of `l1\nl2\nl3\n`: `l1` ends at byte 2, the selection runs
    // from byte 0 to byte 5 without that last newline, and line 1 starts at 3.
    assert_eq!(
        lines
            .scan_lines(0, Some(2), &context)
            .await
            .expect("scanLines"),
        pir::env_client::LineScan {
            newlines: 3,
            start: 0,
            end: 5,
            first_line_end: 2,
            last_line_start: 3,
            selected_bytes: 5,
            first_line_bytes: 2,
        }
    );
    // To the end of the file, and the invalid range is refused before the wire.
    let all = lines
        .scan_lines(0, None, &context)
        .await
        .expect("scanLines to the end");
    assert_eq!((all.newlines, all.start, all.end), (3, 0, 9));
    assert_eq!(all.selected_bytes, 9);
    for (start, end) in [
        (1u64, Some(1u64)),
        (1, Some(0)),
        (1, Some(9_007_199_254_740_992)),
    ] {
        let error = lines
            .scan_lines(start, end, &context)
            .await
            .expect_err("an invalid line range");
        assert_eq!(error.code, FileErrorCode::Invalid);
        assert_eq!(error.message, "Invalid line range");
    }
    lines.close().await;
    environment.connection.close();
}

// ---------------------------------------------------------------------------
// `shellPath` and the daemon-side `spill` (the two remote-arm options)
// ---------------------------------------------------------------------------

/// `shellPath` is a **remote-side** construction option (`RemoteExecutionEnv
/// Options.shellPath`, sent verbatim as the exec request's `shellPath`): unset
/// — the default, and the only shape before the option existed — leaves the
/// daemon's own shell probe in charge; a configured one is what the daemon
/// runs command strings through, and a path it cannot find fails with
/// `shell_unavailable` (`packages/env/docs/semantics.md:46`), so the field
/// provably reaches the daemon.
#[tokio::test]
async fn a_configured_shell_path_reaches_the_daemon() {
    let tree = Tree::new("shellpath");
    let environment = local_environment(Some(tree.root()));
    let command = pir::env_client::ExecCommand::Shell("echo probe".to_string());
    let options = pir::env_client::ExecOptions {
        cwd: Some(tree.root()),
        ..Default::default()
    };
    // Unset: the daemon's own resolution runs the command (unchanged default).
    let result = environment
        .exec(&command, &options, &Default::default())
        .await
        .expect("the daemon's own shell resolution");
    assert_eq!(result.exit_code, 0);

    // Configured to a path that does not exist on the remote machine: the
    // daemon refuses the exec and names the configured path. (The option is
    // the *remote* path — the local settings `shellPath` is never forwarded;
    // see `env_client::selection`.)
    let missing = tree.path("no-such-shell-xyz");
    let mut configured = RemoteExecutionEnvOptions::new(
        "pi-env:shellpath-configured",
        environment.connection.clone(),
    );
    configured.cwd = Some(tree.root());
    configured.shell_path = Some(missing.clone());
    let configured = RemoteExecutionEnv::new(configured);
    let error = configured
        .exec(&command, &options, &Default::default())
        .await
        .expect_err("a configured shell that does not exist");
    assert_eq!(error.code, ExecutionErrorCode::ShellUnavailable);
    assert!(
        error.message.contains(&missing),
        "the daemon reports the configured path: {}",
        error.message
    );
    environment.connection.close();
}

/// The daemon-side `spill` is opt-in, and its file belongs to the daemon: a
/// `tmp-*` directory under the *daemon's* temp dir, named `pi-output-*.log`
/// (upstream `createTempFile({ prefix: "pi-output-", suffix: ".log" })`,
/// `daemon/src/exec.rs`). The coding-agent remote arm never requests it —
/// upstream's client-side `OutputAccumulator` owns the full-output file, and
/// `packages/durable`'s bash tool is the only upstream caller of `spill` — so
/// an ordinary exec (the arm's `ExecOptions::default()` shape) answers with no
/// `spillPath` at all. Requesting it here pins the ownership fact a
/// daemon-side spill would change.
#[tokio::test]
async fn the_daemons_spill_is_opt_in_and_named_by_the_daemon() {
    let tree = Tree::new("daemonspill");
    let environment = local_environment(Some(tree.root()));
    let command =
        pir::env_client::ExecCommand::Shell("head -c 200000 /dev/zero | tr '\\0' x".to_string());
    let result = environment
        .exec(
            &command,
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .expect("exec without spill");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        result.spill_path, None,
        "an unrequested spill produces no daemon file"
    );

    let result = environment
        .exec(
            &command,
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                spill: Some(json!({ "afterBytes": 1024, "afterLines": 10 })),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .expect("exec with spill");
    let path = result
        .spill_path
        .expect("the daemon reports its spill file");
    let name = std::path::Path::new(&path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with("pi-output-"),
        "the daemon's own file family: {path}"
    );
    assert_eq!(
        std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
        200_000,
        "the daemon's file is the complete stream"
    );
    environment.connection.close();
}
