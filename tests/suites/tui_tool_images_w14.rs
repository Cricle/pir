//! Tool-result image content (audit §7 #1 residual).
//!
//! Tool results used to render text blocks only. These tests pin that their
//! `ImageContent` blocks join the SAME inline-image path assistant images and
//! `/image` use: as-is PNG to Kitty/iTerm2, JPEG/GIF/WebP transcoded to PNG
//! for Kitty (which accepts PNG only), and the upstream `[Image: …]` text
//! fallback when the payload cannot be decoded or the protocol is absent.
//! The transcode applies EXIF orientation, so phone JPEGs reach the terminal
//! upright.

use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use base64::Engine;
use pir::ai::types::ImageContent;
use pir::tui::core::Component;
use pir::tui::messages::ToolExecutionComponent;
use pir::tui::terminal_image::{
    image_content_lines_with_protocol, protocol_can_display_mime, protocol_supports_mime,
    ImageProtocol,
};

const PNG_BASE64_PREFIX: &str = "iVBORw0KGgo";

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A small valid image in `format` (JPEG/GIF/WebP encoders included by the
/// `image` features the TUI already enables).
fn encode_image(format: ::image::ImageFormat) -> Vec<u8> {
    let source = ::image::RgbImage::from_pixel(8, 6, ::image::Rgb([12, 34, 56]));
    let mut bytes = Vec::new();
    ::image::DynamicImage::ImageRgb8(source)
        .write_to(&mut Cursor::new(&mut bytes), format)
        .expect("encode test image");
    bytes
}

/// A JPEG whose EXIF IFD0 carries `Orientation = 6` (rotate 90° clockwise):
/// the stored bitmap is 8×6, the upright image 6×8.
fn jpeg_with_exif_orientation_6() -> Vec<u8> {
    let jpeg = encode_image(::image::ImageFormat::Jpeg);
    assert!(
        jpeg.starts_with(&[0xFF, 0xD8]),
        "the encoder emitted a JPEG SOI"
    );
    // APP1/Exif payload: "Exif\0\0" (zune-jpeg strips it), then a
    // little-endian TIFF header whose IFD0 has one entry — tag 0x0112
    // (Orientation), type 3 (SHORT), count 1, value 6 — and no next IFD.
    let exif_tiff: &[u8] = &[
        0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // TIFF header, IFD0 at offset 8
        0x01, 0x00, // one IFD entry
        0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
        0x00, // Orientation 6
        0x00, 0x00, 0x00, 0x00, // no next IFD
    ];
    let mut payload = b"Exif\x00\x00".to_vec();
    payload.extend_from_slice(exif_tiff);
    // Segment length includes its own two bytes.
    let segment_len = (payload.len() + 2) as u16;
    let mut jpeg_with_exif = Vec::with_capacity(jpeg.len() + payload.len() + 4);
    jpeg_with_exif.extend_from_slice(&jpeg[..2]);
    jpeg_with_exif.extend_from_slice(&[0xFF, 0xE1]);
    jpeg_with_exif.extend_from_slice(&segment_len.to_be_bytes());
    jpeg_with_exif.extend_from_slice(&payload);
    jpeg_with_exif.extend_from_slice(&jpeg[2..]);
    jpeg_with_exif
}

/// The base64 PNG a single-placed Kitty sequence carries.
fn kitty_png_payload(sequence: &str) -> Vec<u8> {
    let (_, rest) = sequence.split_once(';').expect("Kitty params separator");
    let payload = rest.strip_suffix("\x1b\\").expect("Kitty terminator");
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("Kitty PNG base64")
}

/// The transcript lines of a tool block carrying one result image.
fn tool_block_with_image(data: &str, mime_type: &str) -> Vec<String> {
    let component = ToolExecutionComponent::new(
        "screenshot",
        Arc::new(serde_json::json!({ "path": "shot.png" })),
        Arc::new(AtomicBool::new(false)),
    );
    component.set_meta(None, None, None);
    component.set_images(Arc::new(vec![ImageContent {
        data: data.to_string(),
        mime_type: mime_type.to_string(),
    }]));
    component.update_result(Arc::from("read image result"), false, Some(3));
    component.render(80)
}

/// The tool block appends its image rows after the text result; an
/// undecodable payload still renders the upstream text fallback instead of
/// disappearing.
#[test]
fn tool_result_image_blocks_render_after_the_text_result() {
    let garbage = encode_base64(b"definitely not an image");
    let lines = tool_block_with_image(&garbage, "image/png");
    assert!(
        lines.iter().any(|line| line.contains("read image result")),
        "the text result stays: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("[Image: image/png]")),
        "an undecodable image falls back to the text line: {lines:?}"
    );
}

/// A `ChatEntry::ToolExecution` carries its image blocks through the transcript
/// container's rebuild path (the entry → `ToolExecutionComponent` bridge), not
/// just through a directly constructed component.
#[test]
fn tool_execution_entries_carry_their_image_blocks() {
    let garbage = encode_base64(b"not an image either");
    let entries = vec![pir::tui::app::ChatEntry::ToolExecution {
        call_id: Some("call-image".to_string()),
        tool_name: "screenshot".to_string(),
        args: Arc::new(serde_json::json!({ "path": "shot.png" })),
        output: Arc::from("entry result"),
        is_error: false,
        duration_ms: Some(4),
        pending: false,
        details: None,
        started_at: None,
        preview: None,
        images: Arc::new(vec![ImageContent {
            data: garbage,
            mime_type: "image/png".to_string(),
        }]),
    }];
    let mut chat = pir::tui::app::ChatContainer::new();
    chat.set_entries(&entries);
    let lines = chat.render(80);
    assert!(
        lines.iter().any(|line| line.contains("entry result")),
        "the entry's text result renders: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("[Image: image/png]")),
        "the entry's image block renders after the result: {lines:?}"
    );
}

/// `/image` shares the transcode: a JPEG file on a Kitty-configured terminal
/// renders inline as PNG instead of dropping to halfblocks.
#[test]
fn image_command_transcodes_non_png_files_for_kitty() {
    use pir::config::settings::{TerminalImagesMode, TerminalSettings};
    use pir::tui::image_preview::{
        render_image_with_terminal, ImageSettings, DEFAULT_IMAGE_WIDTH_CELLS,
    };

    let dir = std::env::temp_dir().join("pir-image-transcode-test");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("photo.jpg");
    std::fs::write(&path, encode_image(::image::ImageFormat::Jpeg)).expect("write");
    let terminal = TerminalSettings {
        images: Some(TerminalImagesMode::Kitty),
        ..Default::default()
    };
    let settings = ImageSettings {
        width_cells: DEFAULT_IMAGE_WIDTH_CELLS as usize,
        show: true,
    };
    let lines = render_image_with_terminal(&path, 20, settings, Some(&terminal)).expect("render");
    let first = lines.first().cloned().unwrap_or_default();
    assert!(
        first.starts_with("\x1b_G") && first.contains(PNG_BASE64_PREFIX),
        "the JPEG is transcoded to a Kitty PNG placement: {first:.80}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Kitty accepts PNG as-is; iTerm2 receives the original base64 bytes.
#[test]
fn tool_result_png_uses_the_inline_protocol() {
    let png = encode_image(::image::ImageFormat::Png);
    let encoded = encode_base64(&png);
    assert!(protocol_supports_mime(ImageProtocol::Kitty, "image/png"));

    let kitty = image_content_lines_with_protocol(
        Some(ImageProtocol::Kitty),
        &encoded,
        "image/png",
        20,
        Some(10),
    );
    assert!(
        kitty.first().is_some_and(|line| line.starts_with("\x1b_G")),
        "Kitty block: {kitty:?}"
    );

    let iterm = image_content_lines_with_protocol(
        Some(ImageProtocol::Iterm2),
        &encoded,
        "image/png",
        20,
        Some(10),
    );
    assert!(
        iterm
            .last()
            .is_some_and(|line| line.contains("\x1b]1337;File=")),
        "iTerm2 block: {iterm:?}"
    );
}

/// Kitty's graphics protocol only accepts PNG, so JPEG/GIF/WebP result images
/// are transcoded to PNG before encoding. The payload carries the PNG
/// signature (the transcode really ran, not a passthrough).
#[test]
fn kitty_transcodes_jpeg_gif_and_webp_result_images_to_png() {
    for (format, mime) in [
        (::image::ImageFormat::Jpeg, "image/jpeg"),
        (::image::ImageFormat::Gif, "image/gif"),
        (::image::ImageFormat::WebP, "image/webp"),
    ] {
        let encoded = encode_base64(&encode_image(format));
        assert!(
            protocol_can_display_mime(ImageProtocol::Kitty, mime),
            "Kitty transcodes {mime}"
        );
        let lines = image_content_lines_with_protocol(
            Some(ImageProtocol::Kitty),
            &encoded,
            mime,
            20,
            Some(10),
        );
        let first = lines.first().unwrap_or(&String::new()).clone();
        assert!(
            first.starts_with("\x1b_G"),
            "{mime} should render inline: {lines:?}"
        );
        assert!(
            first.contains(PNG_BASE64_PREFIX),
            "{mime} should be sent as PNG base64: {first:.80}"
        );
    }
}

/// A corrupt non-PNG payload cannot be transcoded: the text fallback replaces
/// it (never a Kitty sequence with bytes the terminal would reject).
#[test]
fn kitty_transcode_failure_falls_back_to_text() {
    let garbage = encode_base64(b"\xff\xd8 not really a jpeg");
    let lines = image_content_lines_with_protocol(
        Some(ImageProtocol::Kitty),
        &garbage,
        "image/jpeg",
        20,
        None,
    );
    assert_eq!(lines, vec!["[Image: image/jpeg]".to_string()]);
}

/// iTerm2 renders JPEG as-is (no transcode): the sequence carries the
/// original payload.
#[test]
fn iterm2_passes_jpeg_through_unchanged() {
    let encoded = encode_base64(&encode_image(::image::ImageFormat::Jpeg));
    let lines = image_content_lines_with_protocol(
        Some(ImageProtocol::Iterm2),
        &encoded,
        "image/jpeg",
        20,
        None,
    );
    let last = lines.last().cloned().unwrap_or_default();
    assert!(
        last.contains(&format!(":{encoded}\x07")),
        "the original JPEG base64 travels unchanged: {last:.80}"
    );
}

/// The Kitty PNG transcode applies EXIF orientation (upstream Photon and the
/// image tools' `resizeImageInProcess` do): an 8×6 stored bitmap tagged
/// `Orientation = 6` must arrive upright as 6×8, not sideways.
#[test]
fn kitty_transcode_applies_exif_orientation() {
    let encoded = encode_base64(&jpeg_with_exif_orientation_6());
    let lines = image_content_lines_with_protocol(
        Some(ImageProtocol::Kitty),
        &encoded,
        "image/jpeg",
        20,
        None,
    );
    let first = lines.first().cloned().unwrap_or_default();
    assert!(
        first.starts_with("\x1b_G") && first.contains(PNG_BASE64_PREFIX),
        "the JPEG is transcoded to a Kitty PNG placement: {first:.80}"
    );
    let png = kitty_png_payload(&first);
    let upright = ::image::load_from_memory(&png).expect("decode the transcoded PNG");
    assert_eq!(
        (upright.width(), upright.height()),
        (6, 8),
        "Orientation=6 rotates the 8x6 stored bitmap upright"
    );
}
