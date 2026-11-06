//! Interactive `@file` image attachments resize; print-mode expansion stays
//! a byte-for-byte pass-through.
//!
//! The fixture PNGs are assembled by hand (IHDR/IDAT/IEND chunks with zlib
//! "stored" blocks, CRC-32 and Adler-32 computed inline), following
//! `tests/suites/read_image_tool.rs`, so these tests need no image
//! dev-dependency.

use std::path::{Path, PathBuf};

use pir::coding_agent::context::{
    expand_prompt_file_args, expand_prompt_file_args_with_options, ImageAttachmentOptions,
};
use pir::coding_agent::print::{expand_print_prompt_file_args, PRINT_IMAGE_ATTACHMENT_OPTIONS};
use pir::config::settings::{Settings, SettingsManager};

// ---------------------------------------------------------------------------
// Fixture construction (same shape as tests/suites/read_image_tool.rs)
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
    let dir = std::env::temp_dir().join(format!("pir-image-attachment-{name}-{stamp}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("image data must be valid base64")
}

/// `(width, height)` from a PNG IHDR or from a JPEG SOF marker.
fn image_dimensions(bytes: &[u8]) -> (u32, u32) {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        assert!(bytes.len() >= 24, "truncated PNG");
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return (width, height);
    }
    assert!(
        bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8,
        "expected a PNG or JPEG attachment"
    );
    let mut offset = 2;
    while offset + 9 < bytes.len() {
        assert_eq!(bytes[offset], 0xFF, "malformed JPEG marker");
        let marker = bytes[offset + 1];
        if marker == 0xD8 || marker == 0xD9 {
            offset += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u32;
            return (width, height);
        }
        offset += 2 + length;
    }
    panic!("no image dimensions found");
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn interactive_options() -> ImageAttachmentOptions {
    ImageAttachmentOptions {
        max_width: 2000,
        max_height: 2000,
        max_bytes: 4_500_000,
        jpeg_quality: 80,
    }
}

fn gradient(x: u32, _y: u32) -> [u8; 3] {
    [
        (x % 256) as u8,
        ((x * 3) % 256) as u8,
        ((x * 7) % 256) as u8,
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn interactive_expansion_resizes_large_png() {
    let dir = temp_dir("resize");
    let source = png(3000, 1000, gradient);
    let path = write_file(&dir, "big.png", &source);

    let (text, images) =
        expand_prompt_file_args_with_options("@big.png", &dir, Some(interactive_options()))
            .expect("expand @big.png");

    assert_eq!(images.len(), 1, "one image attachment");
    assert!(
        text.contains(&format!("<file name=\"{}\">", path.display())),
        "image marker line kept, got {text:?}"
    );
    let decoded = base64_decode(&images[0].data);
    let (width, height) = image_dimensions(&decoded);
    assert!(
        width <= 2000 && height <= 2000,
        "resized image must fit 2000x2000, got {width}x{height}"
    );
    assert!(
        width < 3000 && height < 1000,
        "resized image must be smaller than the 3000x1000 source, got {width}x{height}"
    );
    assert!(
        decoded.len() < source.len(),
        "resized payload ({} bytes) must be smaller than the source ({} bytes)",
        decoded.len(),
        source.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_expansion_stays_byte_for_byte_passthrough() {
    let dir = temp_dir("print");
    let source = png(3000, 1000, gradient);
    write_file(&dir, "big.png", &source);

    let (_, images) = expand_prompt_file_args("@big.png", &dir).expect("expand @big.png");

    assert_eq!(images.len(), 1, "one image attachment");
    assert_eq!(images[0].mime_type, "image/png");
    let decoded = base64_decode(&images[0].data);
    assert_eq!(decoded, source, "print mode must attach the original bytes");
    assert_eq!(image_dimensions(&decoded), (3000, 1000));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_image_attachment_options_match_the_resize_profile() {
    assert_eq!(PRINT_IMAGE_ATTACHMENT_OPTIONS, interactive_options());
}

#[test]
fn settings_auto_resize_defaults_to_true_and_parses_false() {
    assert!(
        Settings::default().image_auto_resize(),
        "unset settings default autoResize to true"
    );

    let unset: Settings = serde_json::from_str(r#"{"images":{}}"#).expect("parse settings");
    assert!(
        unset.image_auto_resize(),
        "images without autoResize defaults true"
    );

    let enabled: Settings =
        serde_json::from_str(r#"{"images":{"autoResize":true}}"#).expect("parse settings");
    assert!(enabled.image_auto_resize());

    let disabled: Settings =
        serde_json::from_str(r#"{"images":{"autoResize":false}}"#).expect("parse settings");
    assert!(!disabled.image_auto_resize(), "explicit false is honored");
}

#[test]
fn print_auto_resize_follows_settings_manager() {
    let dir = temp_dir("print-settings");
    let source = png(3000, 1000, gradient);
    write_file(&dir, "big.png", &source);
    let global = dir.join("global");
    let project = dir.join("project");
    std::fs::create_dir_all(&global).expect("create global dir");
    std::fs::create_dir_all(&project).expect("create project dir");

    // No settings files: autoResize defaults to true and the print helper
    // resizes the attachment.
    let manager = SettingsManager::load(&global, &project, &dir, true);
    assert!(manager.settings.image_auto_resize());
    let (_, images) =
        expand_print_prompt_file_args("@big.png", &dir, manager.settings.image_auto_resize())
            .expect("expand with default settings");
    assert_eq!(images.len(), 1);
    let (width, height) = image_dimensions(&base64_decode(&images[0].data));
    assert!(
        width <= 2000 && height <= 2000 && width < 3000,
        "autoResize=true must resize, got {width}x{height}"
    );

    // Explicit `images.autoResize: false` keeps the upstream print-mode
    // byte-for-byte pass-through.
    std::fs::write(
        global.join("settings.json"),
        r#"{"images":{"autoResize":false}}"#,
    )
    .expect("write global settings");
    let manager = SettingsManager::load(&global, &project, &dir, true);
    assert!(!manager.settings.image_auto_resize());
    let (_, images) =
        expand_print_prompt_file_args("@big.png", &dir, manager.settings.image_auto_resize())
            .expect("expand with autoResize=false");
    assert_eq!(images.len(), 1);
    assert_eq!(
        base64_decode(&images[0].data),
        source,
        "autoResize=false must attach the original bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn text_and_empty_files_keep_their_behavior_with_options() {
    let dir = temp_dir("text");
    write_file(&dir, "notes.txt", b"hello\n");
    write_file(&dir, "empty.txt", b"");

    let (text, images) = expand_prompt_file_args_with_options(
        "@empty.txt @notes.txt",
        &dir,
        Some(interactive_options()),
    )
    .expect("expand text files");

    assert!(images.is_empty(), "text/empty files attach no images");
    assert!(
        text.contains("<file name=\"") && text.contains(">\nhello\n") && text.contains("\n</file>"),
        "text file rendered unchanged, got {text:?}"
    );
    assert!(
        !text.contains("empty.txt"),
        "empty file stays skipped, got {text:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
