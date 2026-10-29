//! Read tool image support (upstream `core/tools/read.ts` + `processImage`):
//! jpg/png/gif/webp/bmp files come back as IMAGE content blocks (decoded,
//! resized to the inline limits, base64-encoded) instead of the old "binary
//! file" rejection. Text files and non-image binaries keep their previous
//! behavior.
//!
//! The fixture images are constructed by hand (PNG chunks with zlib "stored"
//! blocks, CRC-32 and Adler-32 computed inline) so the tests need no image
//! dependency of their own.

use std::path::PathBuf;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::ai::types::ContentBlock;
use pir::coding_agent::tools::register_core_tools;

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// zlib-compress (stored, uncompressed deflate blocks) `raw` bytes.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for chunk in raw.chunks(u16::MAX as usize) {
        out.push(0x00); // final-bit 0, stored block
        if chunk.len() == raw.len() {
            let len = out.len();
            out[len - 1] = 0x01; // mark the only block final
        }
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Minimal RGB PNG whose pixels form a horizontal gradient (`pixel` maps
/// x/y to an RGB triple).
fn png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width * height * 3 + height) as usize);
    for y in 0..height {
        raw.push(0); // filter type: None
        for x in 0..width {
            raw.extend_from_slice(&pixel(x, y));
        }
    }
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // depth 8, RGB, no interlace
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-read-image-{name}-{stamp}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("image data must be valid base64")
}

/// `(width, height)` from a PNG IHDR (bytes 16..24), for verifying the
/// processed image's real dimensions.
fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert!(bytes.len() >= 24, "not a PNG: too short");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "missing PNG signature");
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width, height)
}

// ---------------------------------------------------------------------------
// Tool invocation
// ---------------------------------------------------------------------------

fn read_tool() -> std::sync::Arc<dyn pir::agent::tool_execution::AgentTool> {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry.get("read").expect("read tool is registered")
}

fn run_read(dir: &std::path::Path, file_name: &str) -> ToolResult {
    let tool = read_tool();
    let ctx = ToolContext::new(dir.to_path_buf());
    let args = serde_json::json!({ "path": file_name });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move { tool.run(&args, &ctx).await })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn png_read_returns_an_image_content_block() {
    let dir = temp_dir("png");
    let fixture = png(4, 3, |x, y| [(x * 40) as u8, (y * 60) as u8, 200]);
    let path = dir.join("tiny.png");
    std::fs::write(&path, &fixture).expect("write fixture");

    let result = run_read(&dir, "tiny.png");
    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert_eq!(result.images.len(), 1, "expected exactly one image block");

    let image = &result.images[0];
    assert_eq!(image.mime_type, "image/png");
    let decoded = base64_decode(&image.data);
    // Passing through unchanged keeps the fixture bytes and dimensions.
    assert_eq!(decoded, fixture, "an in-limit PNG must pass through as-is");
    assert_eq!(png_dimensions(&decoded), (4, 3));

    // The text note names the processed type; no hints for a passthrough.
    assert_eq!(result.content, "Read image file [image/png]");

    // Structured output mirrors upstream `toReadOutput`'s image object.
    let structured = result
        .structured_content
        .expect("image reads carry structured content");
    assert_eq!(structured["type"], "image");
    assert_eq!(structured["mimeType"], "image/png");
    assert_eq!(structured["data"], serde_json::json!(image.data));
    assert_eq!(structured["note"], serde_json::json!(result.content));
}

#[test]
fn oversized_image_is_resized_to_the_inline_limits() {
    let dir = temp_dir("oversized");
    // 2001 px wide is one past the 2000 px inline width limit, so the tool
    // must resize before sending.
    let fixture = png(2001, 1, |x, _| [(x % 256) as u8, 128, 64]);
    let path = dir.join("wide.png");
    std::fs::write(&path, &fixture).expect("write fixture");

    let result = run_read(&dir, "wide.png");
    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert_eq!(result.images.len(), 1, "expected exactly one image block");

    let image = &result.images[0];
    let decoded = base64_decode(&image.data);
    let (width, height) = match image.mime_type.as_str() {
        "image/png" => png_dimensions(&decoded),
        other => panic!("expected a PNG candidate to fit, got {other}"),
    };
    assert_eq!((width, height), (2000, 1), "must be resized below 2000px");

    // Upstream `formatDimensionNote` tells the model how to map coordinates.
    assert!(
        result
            .content
            .contains("[Image: original 2001x1, displayed at 2000x1."),
        "missing the dimension hint: {}",
        result.content
    );
    assert!(
        result.content.starts_with("Read image file [image/png]"),
        "unexpected note: {}",
        result.content
    );
}

#[test]
fn every_supported_signature_sniffs_as_an_image() {
    // JPEG, GIF and WebP heads need only the magic bytes for sniffing; an
    // undecodable body then takes the "could not be converted" path, which is
    // still an image (not the binary-file rejection).
    let dir = temp_dir("signatures");
    for (name, head) in [
        ("photo.jpg", vec![0xFF, 0xD8, 0xFF, 0xE0]),
        ("anim.gif", b"GIF89a".to_vec()),
        ("pic.webp", {
            let mut bytes = b"RIFF".to_vec();
            bytes.extend_from_slice(&[0, 0, 0, 0]);
            bytes.extend_from_slice(b"WEBP");
            bytes
        }),
    ] {
        std::fs::write(dir.join(name), &head).expect("write fixture");
        let result = run_read(&dir, name);
        assert!(
            !result.is_error,
            "{name} must be treated as an image, got error: {}",
            result.content
        );
        assert!(
            result.content.contains("Read image file ["),
            "{name} note: {}",
            result.content
        );
        assert!(
            !result.content.contains("binary file"),
            "{name} must not be rejected as binary: {}",
            result.content
        );
    }
}

#[test]
fn non_image_binary_is_still_rejected() {
    let dir = temp_dir("binary");
    // NUL byte in the head: a binary file that is not an image.
    let path = dir.join("blob.bin");
    std::fs::write(&path, b"\x02\x00\x01\x03\xff\xfe\x00\x10").expect("write fixture");

    let result = run_read(&dir, "blob.bin");
    assert!(result.is_error, "binary files must still be rejected");
    assert!(
        result.content.contains("binary file"),
        "unexpected error: {}",
        result.content
    );
    assert!(result.images.is_empty());
}

#[test]
fn text_files_are_unchanged() {
    let dir = temp_dir("text");
    let path = dir.join("notes.txt");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").expect("write fixture");

    let result = run_read(&dir, "notes.txt");
    assert!(!result.is_error, "unexpected error: {}", result.content);
    // Upstream splits on "\n" (a trailing newline is a final empty piece) and
    // joins the selected slice back, so the output keeps the trailing newline.
    assert_eq!(result.content, "alpha\nbeta\ngamma\n");
    assert!(result.images.is_empty());
    // Upstream `toReadOutput`: a text read's structured content is the text
    // itself.
    assert_eq!(
        result.structured_content,
        Some(serde_json::Value::String(
            "alpha
beta
gamma
"
            .to_owned()
        ))
    );
}

#[test]
fn text_offset_and_limit_behavior_is_preserved() {
    let dir = temp_dir("offset");
    let path = dir.join("lines.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").expect("write fixture");

    let tool = read_tool();
    let ctx = ToolContext::new(dir.to_path_buf());
    let args = serde_json::json!({ "path": "lines.txt", "offset": 2, "limit": 2 });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime.block_on(async move { tool.run(&args, &ctx).await });

    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert!(
        result.content.starts_with("two\nthree"),
        "unexpected slice: {}",
        result.content
    );
    assert!(
        result.content.contains("Use offset=4 to continue."),
        "missing continuation hint: {}",
        result.content
    );
}

#[test]
fn non_vision_models_get_the_omitted_note() {
    // Upstream `getNonVisionImageNote`: the block is still returned, with a
    // note that the model cannot receive it (the adapter transform drops it).
    let dir = temp_dir("nonvision");
    let path = dir.join("tiny.png");
    std::fs::write(&path, png(2, 2, |_, _| [1, 2, 3])).expect("write fixture");

    // `session_env` is None here, so the note is absent - the transform layer
    // (pinned separately) is what enforces the downgrade.
    let result = run_read(&dir, "tiny.png");
    assert!(!result.is_error);
    assert_eq!(result.images.len(), 1);
    assert_eq!(
        result.images[0].mime_type, "image/png",
        "the block itself is always emitted"
    );
    assert!(
        !result.content.contains("does not support images"),
        "no model context means no vision note: {}",
        result.content
    );
}

#[test]
fn image_compatibility_matches_content_block_shape() {
    // The emitted image is a real `ContentBlock::Image` payload, so the
    // adapter/transform layer can carry or downgrade it.
    let dir = temp_dir("block");
    std::fs::write(dir.join("tiny.png"), png(3, 1, |_, _| [9, 9, 9])).expect("write fixture");
    let result = run_read(&dir, "tiny.png");
    assert_eq!(result.images.len(), 1);

    let block = ContentBlock::Image(result.images[0].clone());
    let encoded = serde_json::to_value(&block).expect("serializes");
    assert_eq!(encoded["type"], "image");
    assert_eq!(encoded["mimeType"], "image/png");
    assert!(encoded["data"]
        .as_str()
        .is_some_and(|data| !data.is_empty()));
}
