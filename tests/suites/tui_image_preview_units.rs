//! Moved inline unit tests for the halfblocks image preview renderer.
//!
//! The renderer lives behind the `image-protocol` cargo feature, which is part
//! of `full` (the default) — so these tests run in the default
//! `cargo test`/`cargo test --test g_tui` run. A slim TUI build without the
//! feature compiles this crate out (the renderer does not exist there);
//! `render_image_with_settings` then renders the text fallback, covered by the
//! feature-off unit test in `src/tui/image_preview.rs`.
//!
//! The fixture PNG is constructed by hand (zlib "stored" blocks, CRC-32 and
//! Adler-32 computed inline) so no additional dev-dependency is needed.

#![cfg(feature = "image-protocol")]

use std::path::Path;

use pir::tui::image_preview::{
    image_fallback_line, render_image_halfblocks, sniff_image_dimensions,
};

/// zlib Adler-32 checksum over raw bytes.
fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Standard CRC-32 (IEEE 802.3, as required by PNG chunks).
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
        let last = chunk.len() == raw.len();
        if last {
            let len = out.len();
            out[len - 1] = 0x01; // mark final block
        }
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Build a minimal RGBA PNG with every pixel set to `rgba`.
fn tiny_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width * height * 4 + height) as usize);
    for _ in 0..height {
        raw.extend(std::iter::repeat_n(0, 1)); // filter type: None
        for _ in 0..width {
            raw.extend_from_slice(&rgba);
        }
    }
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // depth 8, RGBA, no interlace
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut png, b"IEND", &[]);
    png
}

#[test]
fn renders_halfblocks_within_bounds() {
    let dir = std::env::temp_dir().join("pir-image-preview-test");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("tiny.png");
    std::fs::write(&path, tiny_png(4, 4, [255u8, 0, 0, 255])).expect("write");
    let lines = render_image_halfblocks(&path, 20, 10).expect("renders");
    assert!(!lines.is_empty());
    assert!(lines.len() <= 10);
    for line in &lines {
        assert!(line.contains('▀'));
    }
}

#[test]
fn decode_failures_fall_back_to_the_text_line_not_panics() {
    // Missing file: the upstream `[Image: ...]` fallback replaces the error.
    let error = render_image_halfblocks(Path::new("definitely-missing.png"), 10, 5);
    let lines = error.expect("falls back instead of erroring");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("[Image: "));
}

/// Minimal JPEG stream: FF D8, an APP0 segment, then an SOF0 segment
/// (length 11, precision 8, height, width — the sniffer reads the SOF0
/// layout).
fn tiny_jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut jpeg = vec![0xFF, 0xD8];
    // APP0 segment (length 16 = 2 length bytes + 14 payload).
    jpeg.extend_from_slice(&[0xFF, 0xE0]);
    jpeg.extend_from_slice(&16u16.to_be_bytes());
    jpeg.extend_from_slice(&[0u8; 14]);
    // SOF0 marker: segment length, precision, then height and width.
    jpeg.extend_from_slice(&[0xFF, 0xC0]);
    jpeg.extend_from_slice(&11u16.to_be_bytes());
    jpeg.push(0x08);
    jpeg.extend_from_slice(&height.to_be_bytes());
    jpeg.extend_from_slice(&width.to_be_bytes());
    // Trailing entropy data: the sniffer's scan bound is
    // `offset < len - 9`, so the SOF0 must not sit in the last 9 bytes
    // (matching upstream's loop bound).
    jpeg.extend_from_slice(&[0u8; 8]);
    jpeg
}

/// Minimal GIF87a header with the logical screen size.
fn tiny_gif(width: u16, height: u16) -> Vec<u8> {
    let mut gif = b"GIF87a".to_vec();
    gif.extend_from_slice(&width.to_le_bytes());
    gif.extend_from_slice(&height.to_le_bytes());
    gif.extend_from_slice(&[0u8; 5]);
    gif
}

/// Minimal RIFF/WEBP-VP8 header with the frame dimensions.
fn tiny_webp_vp8(width: u16, height: u16) -> Vec<u8> {
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&0u32.to_le_bytes());
    webp.extend_from_slice(b"WEBP");
    webp.extend_from_slice(b"VP8 ");
    // Pad to offset 26, then the (masked) dimensions.
    while webp.len() < 26 {
        webp.push(0);
    }
    webp.extend_from_slice(&(width & 0x3FFF).to_le_bytes());
    webp.extend_from_slice(&(height & 0x3FFF).to_le_bytes());
    webp
}

#[test]
fn fallback_sniffs_dimensions_from_headers() {
    assert_eq!(
        sniff_image_dimensions(&tiny_png(640, 480, [0; 4])),
        Some((640, 480))
    );
    assert_eq!(
        sniff_image_dimensions(&tiny_jpeg(1024, 768)),
        Some((1024, 768))
    );
    assert_eq!(
        sniff_image_dimensions(&tiny_gif(320, 200)),
        Some((320, 200))
    );
    assert_eq!(
        sniff_image_dimensions(&tiny_webp_vp8(64, 32)),
        Some((64, 32))
    );
    assert_eq!(sniff_image_dimensions(b"not an image"), None);
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows chord glyphs")]
fn fallback_line_matches_upstream_shape() {
    let dir = std::env::temp_dir().join("pir-image-fallback-test");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("broken.png");
    // PNG magic + dimensions, but garbage after (undecodable).
    let mut broken = tiny_png(12, 34, [0; 4]);
    broken.truncate(30);
    std::fs::write(&path, &broken).expect("write");
    let line = render_image_halfblocks(&path, 60, 20)
        .expect("falls back instead of erroring")
        .pop()
        .expect("one line");
    // Shape: [Image: <~/path> [<mime>] <WxH> (<failure reason>)] — the
    // home-prefixed temp path is shortened (upstream shortenImagePath).
    let shortened = path.display().to_string().replacen(
        std::env::var("USERPROFILE").as_deref().unwrap_or(""),
        "~",
        1,
    );
    assert!(
        line.starts_with(&format!("[Image: {shortened} [image/png] 12x34 (")),
        "actual line: {line}"
    );
    assert!(line.ends_with(")]"));
    // The direct constructor gives the same shape (unknown mime for garbage).
    let plain = image_fallback_line(Path::new("blob.bin"), Some(b"garbage"), "nope");
    assert_eq!(plain, "[Image: blob.bin [image/unknown] (nope)]");
    // Oversized images keep their sniffed dimensions in the fallback.
    let huge = image_fallback_line(Path::new("big.gif"), Some(&tiny_gif(8, 9)), "too large");
    assert_eq!(huge, "[Image: big.gif [image/gif] 8x9 (too large)]");
}

// ---------------------------------------------------------------------------
// Inline protocol output (upstream `terminal-image.ts` encoders, audit §7 #1)
// ---------------------------------------------------------------------------

use pir::tui::terminal_image::{
    allocate_image_id, calculate_image_cell_size, encode_iterm2, encode_kitty, image_block_lines,
    is_image_line, protocol_supports_mime, Iterm2EncodeOptions, KittyEncodeOptions,
};

#[test]
fn kitty_only_accepts_png_while_iterm2_accepts_common_types() {
    use pir::tui::terminal_image::ImageProtocol;
    assert!(protocol_supports_mime(ImageProtocol::Kitty, "image/png"));
    assert!(!protocol_supports_mime(ImageProtocol::Kitty, "image/jpeg"));
    assert!(protocol_supports_mime(ImageProtocol::Iterm2, "image/jpeg"));
    assert!(protocol_supports_mime(ImageProtocol::Iterm2, "image/webp"));
    assert!(!protocol_supports_mime(ImageProtocol::Iterm2, "image/bmp"));
}

#[test]
fn kitty_encoder_matches_the_upstream_sequence_shape() {
    let sequence = encode_kitty(
        "QUJD",
        KittyEncodeOptions {
            columns: Some(4),
            rows: Some(2),
            image_id: Some(7),
            move_cursor: false,
        },
    );
    // `a=T,f=100,q=2,C=1,c=4,r=2,i=7;DATA` terminated by ST.
    assert_eq!(sequence, "\x1b_Ga=T,f=100,q=2,C=1,c=4,r=2,i=7;QUJD\x1b\\");
    assert!(is_image_line(&sequence));

    // >4096 bytes chunk with `m=1` continuations and a final `m=0`.
    let data = "A".repeat(4096 * 2 + 10);
    let chunked = encode_kitty(
        &data,
        KittyEncodeOptions {
            columns: None,
            rows: None,
            image_id: None,
            move_cursor: true,
        },
    );
    assert!(chunked.starts_with("\x1b_Ga=T,f=100,q=2,m=1;"));
    // Two non-final chunks carry m=1 (the first has the params, the middle
    // chunk is bare), the last carries m=0.
    assert_eq!(chunked.matches("m=1;").count(), 2);
    assert_eq!(chunked.matches("m=0;").count(), 1);
    assert!(chunked.contains("\x1b_Gm=0;"));
    assert!(chunked.ends_with("\x1b\\"));
    // move_cursor=true omits the C=1 parameter.
    assert!(!chunked.contains("C=1"));
}

#[test]
fn iterm2_encoder_carries_size_width_and_name() {
    let sequence = encode_iterm2(
        "QUJD", // 3 decoded bytes
        &Iterm2EncodeOptions {
            width: Some("4".to_string()),
            height: Some("auto".to_string()),
            name: Some("a.png".to_string()),
            preserve_aspect_ratio: true,
        },
    );
    // name is base64 ("YS5wbmc="), inline=1 and the decoded size.
    assert_eq!(
        sequence,
        "\x1b]1337;File=inline=1;size=3;width=4;height=auto;name=YS5wbmc=:QUJD\x07"
    );
    assert!(is_image_line(&sequence));
    // Padding is excluded from `size`.
    let padded = encode_iterm2("QQ==", &Iterm2EncodeOptions::default());
    assert!(padded.contains(";size=1;"), "{padded}");
    assert!(is_image_line(&format!("\x1b[2A{padded}")));
    assert!(!is_image_line("plain text"));
}

#[test]
fn cell_size_matches_the_upstream_formula() {
    // A 90x90 image at 9x18 cells: width-limited to 10 columns, 5 rows.
    assert_eq!(calculate_image_cell_size(90, 90, 10, None, false), (10, 5));
    // Height cap limits the scale, so the width shrinks with it (upstream
    // `calculateImageCellSize`: scale = min(widthScale, heightScale)).
    assert_eq!(
        calculate_image_cell_size(90, 90, 10, Some(2), false),
        (4, 2)
    );
    // Zero/unknown sizes still reserve at least one cell.
    assert_eq!(calculate_image_cell_size(0, 0, 1, None, false), (1, 1));
    assert!(allocate_image_id() > 0);
}

#[test]
fn image_block_lines_shape_matches_the_protocol() {
    // Kitty: sequence on the first line, then rows-1 empty reservations.
    let kitty = image_block_lines(
        "QUJD",
        90,
        90,
        pir::tui::terminal_image::ImageProtocol::Kitty,
        10,
        None,
    )
    .expect("renders");
    assert_eq!(kitty.len(), 5);
    assert!(kitty[0].starts_with("\x1b_G"));
    assert!(kitty[1..].iter().all(String::is_empty));

    // iTerm2: leading reservations, then cursor-up + sequence.
    let iterm = image_block_lines(
        "QUJD",
        90,
        90,
        pir::tui::terminal_image::ImageProtocol::Iterm2,
        10,
        None,
    )
    .expect("renders");
    assert_eq!(iterm.len(), 5);
    assert!(iterm[..4].iter().all(String::is_empty));
    assert!(iterm[4].starts_with("\x1b[4A\x1b]1337;File="));

    // Unusable input never panics.
    assert!(image_block_lines(
        "",
        0,
        0,
        pir::tui::terminal_image::ImageProtocol::Kitty,
        10,
        None
    )
    .is_none());
}

/// `/image` prefers the configured inline protocol over halfblocks (audit
/// §7 #1): a Kitty-configured terminal emits a Kitty sequence on the first
/// row; an iTerm2-configured one emits the OSC 1337 block on the last row.
#[test]
fn image_preview_uses_the_configured_inline_protocol() {
    use pir::config::settings::{TerminalImagesMode, TerminalSettings};
    use pir::tui::image_preview::{
        render_image_with_terminal, ImageSettings, DEFAULT_IMAGE_WIDTH_CELLS,
    };

    let dir = std::env::temp_dir().join("pir-image-protocol-test");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("inline.png");
    std::fs::write(&path, tiny_png(18, 18, [1u8, 2, 3, 255])).expect("write");
    let settings = ImageSettings {
        width_cells: DEFAULT_IMAGE_WIDTH_CELLS as usize,
        show: true,
    };

    let kitty_terminal = TerminalSettings {
        images: Some(TerminalImagesMode::Kitty),
        ..Default::default()
    };
    let lines = render_image_with_terminal(&path, 20, settings, Some(&kitty_terminal))
        .expect("kitty render");
    assert!(lines[0].starts_with("\x1b_G"), "first row: {:?}", lines[0]);
    assert!(lines[1..].iter().all(String::is_empty));

    let iterm_terminal = TerminalSettings {
        images: Some(TerminalImagesMode::Iterm2),
        ..Default::default()
    };
    let lines = render_image_with_terminal(&path, 20, settings, Some(&iterm_terminal))
        .expect("iterm2 render");
    let last = lines.last().expect("rows");
    assert!(last.contains("\x1b]1337;File="), "last row: {last:?}");

    // `show = false` falls back to the text line even with a protocol.
    let hidden = ImageSettings {
        width_cells: DEFAULT_IMAGE_WIDTH_CELLS as usize,
        show: false,
    };
    let lines =
        render_image_with_terminal(&path, 20, hidden, Some(&kitty_terminal)).expect("fallback");
    assert!(lines[0].starts_with("[Image:"), "{:?}", lines[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An `image` content block without a usable protocol renders the upstream
/// `[Image: <mime> <WxH>]` line (the transcript image-block fallback).
#[test]
fn image_content_block_falls_back_without_a_protocol() {
    use base64::Engine;
    use pir::tui::terminal_image::{
        image_content_lines, set_resolved_capabilities, TerminalCapabilities,
    };

    set_resolved_capabilities(TerminalCapabilities {
        images: None,
        true_color: true,
        hyperlinks: false,
    });
    let encoded = base64::engine::general_purpose::STANDARD.encode(tiny_png(4, 6, [9, 9, 9, 255]));
    let lines = image_content_lines(&encoded, "image/png", 10, Some(5));
    assert_eq!(lines, vec!["[Image: image/png 4x6]".to_string()]);
}
