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

// ---------------------------------------------------------------------------
// Kitty crop parity — registry + `cropKittyImageLine` (drift rows 15/16)
// ---------------------------------------------------------------------------

use pir::tui::terminal_image::{
    crop_kitty_image_line, encode_kitty as kitty_encode, get_kitty_image_metadata,
    get_kitty_image_placement, get_kitty_image_placement_rows, get_max_kitty_image_rows,
    register_kitty_image_metadata, render_image, KittyImageMetadata,
};

/// Serializes the registry tests: the metadata table is process-global
/// (upstream `kittyImageMetadata` is module-global too). The canonical runner
/// (nextest) isolates tests per process, but `cargo test` shares one.
fn crop_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The upstream fixture (`terminal-image.test.ts:532-568`): image 42 is
/// registered as 3 columns x 3 rows over a 100x100 px payload.
const CROP_FIXTURE_ID: u32 = 42;

fn register_crop_fixture() {
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: CROP_FIXTURE_ID,
        columns: 3,
        rows: 3,
        width_px: 100,
        height_px: 100,
    });
}

fn fixture_transmission() -> String {
    kitty_encode(
        "QUJD",
        KittyEncodeOptions {
            columns: Some(3),
            rows: Some(3),
            image_id: Some(CROP_FIXTURE_ID),
            move_cursor: false,
        },
    )
}

/// Upstream `terminal-image.test.ts:532-549` pins that the crop of the 100x100
/// fixture at `hiddenRows=2, visibleRows=1` emits `y=66,h=34,r=1`; this pins
/// the EXACT bytes of the whole line (controls in upstream order, payload
/// untouched).
#[test]
fn crop_kitty_image_line_pins_the_upstream_source_rectangle_bytes() {
    let _guard = crop_test_lock();
    register_crop_fixture();
    let transmission = fixture_transmission();
    assert_eq!(
        transmission,
        "\x1b_Ga=T,f=100,q=2,C=1,c=3,r=3,i=42;QUJD\x1b\\"
    );

    assert_eq!(
        crop_kitty_image_line(&transmission, 2, 1),
        "\x1b_Ga=T,f=100,q=2,C=1,c=3,i=42,y=66,h=34,r=1;QUJD\x1b\\"
    );
    // Bottom clip (upstream `layout.ts:339-344` crops with hiddenRows=0):
    // 2 of 3 rows visible -> y=0, h=ceil(100*2/3)=67, r=2.
    assert_eq!(
        crop_kitty_image_line(&transmission, 0, 2),
        "\x1b_Ga=T,f=100,q=2,C=1,c=3,i=42,y=0,h=67,r=2;QUJD\x1b\\"
    );
    // A middle slice: hidden 1, visible 1 -> y=floor(100/3)=33,
    // end=ceil(100*2/3)=67, h=34.
    assert_eq!(
        crop_kitty_image_line(&transmission, 1, 1),
        "\x1b_Ga=T,f=100,q=2,C=1,c=3,i=42,y=33,h=34,r=1;QUJD\x1b\\"
    );
}

/// The upstream placement fixture (`terminal-image.test.ts:556-568`): the
/// cropped controls of a chunked transmission become the placement-only
/// command `\x1b_Ga=p,q=2,C=1,c=3,i=42,y=66,h=34,r=1\x1b\`.
#[test]
fn kitty_placement_pins_the_upstream_cropped_control_bytes() {
    let _guard = crop_test_lock();
    register_crop_fixture();
    let transmission = kitty_encode(
        &"A".repeat(8192),
        KittyEncodeOptions {
            columns: Some(3),
            rows: Some(3),
            image_id: Some(CROP_FIXTURE_ID),
            move_cursor: false,
        },
    );
    let line = format!("left {} right", crop_kitty_image_line(&transmission, 2, 1));
    let placement = get_kitty_image_placement(&line).expect("registered image -> placement");
    assert_eq!(
        placement.sequence,
        "\x1b_Ga=p,q=2,C=1,c=3,i=42,y=66,h=34,r=1\x1b\\"
    );
    assert_eq!(
        placement.replacement_line,
        format!("left {} right", placement.sequence)
    );
    assert!(!placement.replacement_line.contains("AAAA"));
    assert_eq!(get_kitty_image_placement_rows(&line), Some(1));
    assert_eq!(
        placement.transmission_bytes,
        line.len() - "left ".len() - " right".len()
    );
    assert_eq!(placement.estimated_decoded_bytes, 100 * 100 * 4);
    assert_eq!(placement.rows, 1);
    assert_eq!(placement.image_id, CROP_FIXTURE_ID);
}

/// Every upstream no-op (`terminal-image.ts:472-477`) returns the line
/// byte-identical.
#[test]
fn crop_noops_return_the_line_byte_identical() {
    let _guard = crop_test_lock();
    register_crop_fixture();
    let registered = fixture_transmission();

    // hiddenRows < 0 / hiddenRows >= rows / visibleRows <= 0 / whole image
    // visible (hiddenRows === 0 && croppedRows === rows).
    for (hidden, visible) in [(-1, 1), (3, 1), (4, 1), (0, 0), (0, 3), (0, 9)] {
        assert_eq!(
            crop_kitty_image_line(&registered, hidden, visible),
            registered,
            "no-op at hiddenRows={hidden}, visibleRows={visible}"
        );
    }
    // No registered metadata for the id.
    let unregistered = kitty_encode(
        "QUJD",
        KittyEncodeOptions {
            columns: Some(3),
            rows: Some(3),
            image_id: Some(0x0C12_7777),
            move_cursor: false,
        },
    );
    assert_eq!(crop_kitty_image_line(&unregistered, 1, 1), unregistered);
    // No `\x1b_G…;` command to rewrite: plain text, an iTerm2 image (upstream
    // has no iTerm2 crop), and a kitty prefix without a terminator semicolon.
    assert_eq!(crop_kitty_image_line("plain text", 1, 1), "plain text");
    let iterm = encode_iterm2("QUJD", &Iterm2EncodeOptions::default());
    assert_eq!(crop_kitty_image_line(&iterm, 1, 1), iterm);
    let no_semicolon = "\x1b_Ga=T,f=100,i=42";
    assert_eq!(crop_kitty_image_line(no_semicolon, 1, 1), no_semicolon);
}

/// The crop drops existing `y=`/`h=`/`r=` controls first, then appends the new
/// three in `y=`, `h=`, `r=` order (upstream `terminal-image.ts:481`), and
/// preserves any prefix/suffix text around the sequence verbatim.
#[test]
fn crop_drops_existing_yhr_controls_then_appends_the_new_ones() {
    let _guard = crop_test_lock();
    register_crop_fixture();
    // A line already carrying source-rectangle controls (a re-crop) - the
    // registry rows (3) drive the math, not the line's r=.
    let line = "\x1b_Ga=T,f=100,q=2,C=1,c=3,i=42,y=7,h=9,r=3;QUJD\x1b\\";
    let cropped = crop_kitty_image_line(line, 2, 1);
    assert_eq!(
        cropped,
        "\x1b_Ga=T,f=100,q=2,C=1,c=3,i=42,y=66,h=34,r=1;QUJD\x1b\\"
    );
    assert_eq!(cropped.matches(",y=").count(), 1, "{cropped:?}");
    assert_eq!(cropped.matches(",h=").count(), 1, "{cropped:?}");
    assert_eq!(cropped.matches(",r=").count(), 1, "{cropped:?}");

    let embedded = format!("left {line} right");
    let string_terminator = "\x1b\\";
    let expected_embedded = format!(
        "left {}{string_terminator} right",
        "\x1b_Ga=T,f=100,q=2,C=1,c=3,i=42,y=66,h=34,r=1;QUJD"
    );
    assert_eq!(crop_kitty_image_line(&embedded, 2, 1), expected_embedded);
}

/// `render_image` registers what the crop reads (upstream
/// `terminal-image.ts:704-713`): id, cell footprint and pixel size.
#[test]
fn render_image_registers_kitty_metadata_for_the_crop() {
    let _guard = crop_test_lock();
    let rendered = render_image(
        "QUJD",
        100,
        200,
        pir::tui::terminal_image::ImageProtocol::Kitty,
        3,
        None,
    )
    .expect("kitty render");
    let image_id = rendered.image_id.expect("kitty placements carry ids");
    let expected = KittyImageMetadata {
        image_id,
        columns: 3,
        rows: 3,
        width_px: 100,
        height_px: 200,
    };
    assert_eq!(get_kitty_image_metadata(&rendered.sequence), Some(expected));
    assert_eq!(rendered.columns, 3);
    assert_eq!(rendered.rows, 3);
    // The crop reads the registered pixel size: 100x200 over 3 rows, hidden 2,
    // 1 visible -> y=floor(200/3)=133, end=200, h=67, and the encoder's own
    // `r=3` is dropped before the new y/h/r trio is appended.
    assert_eq!(
        crop_kitty_image_line(&rendered.sequence, 2, 1),
        format!("\x1b_Ga=T,f=100,q=2,C=1,c=3,i={image_id},y=133,h=67,r=1;QUJD\x1b\\")
    );
    // iTerm2 lines are not registered (upstream registers Kitty only).
    let iterm = render_image(
        "QUJD",
        100,
        200,
        pir::tui::terminal_image::ImageProtocol::Iterm2,
        3,
        None,
    )
    .expect("iterm2 render");
    assert_eq!(get_kitty_image_metadata(&iterm.sequence), None);
    assert!(get_max_kitty_image_rows() >= 3);
}

/// The registry bound (upstream `registerKittyImageMetadata`: more than 1000
/// entries deletes the oldest id) and re-registration (delete + set moves the
/// entry to the end).
#[test]
fn kitty_metadata_registry_evicts_the_oldest_entry_past_its_bound() {
    let _guard = crop_test_lock();
    let line_for = |id: u32| format!("\x1b_Ga=T,f=100,q=2,C=1,c=1,r=1,i={id};QUJD\x1b\\");
    // A marker registered FIRST is the oldest entry this test owns; after
    // 1001 further registrations it must be gone (removals = 1001 - 1000 +
    // whatever entries existed before this test).
    let marker = 0x0C12_C000u32;
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: marker,
        columns: 1,
        rows: 1,
        width_px: 1,
        height_px: 1,
    });
    let base = 0x0C12_D000u32;
    for index in 0..1001u32 {
        register_kitty_image_metadata(KittyImageMetadata {
            image_id: base + index,
            columns: 1,
            rows: 1,
            width_px: 1,
            height_px: 1,
        });
    }
    assert_eq!(
        get_kitty_image_metadata(&line_for(marker)),
        None,
        "oldest evicted"
    );
    assert!(
        get_kitty_image_metadata(&line_for(base + 1000)).is_some(),
        "newest kept"
    );
    // Re-registration replaces in place.
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: base + 1000,
        columns: 2,
        rows: 2,
        width_px: 4,
        height_px: 6,
    });
    assert_eq!(
        get_kitty_image_metadata(&line_for(base + 1000)),
        Some(KittyImageMetadata {
            image_id: base + 1000,
            columns: 2,
            rows: 2,
            width_px: 4,
            height_px: 6,
        })
    );
}

// ---------------------------------------------------------------------------
// Scrolled-painter crop (`core.rs::render_scrolled`, drift row 16)
// ---------------------------------------------------------------------------

/// Pre-built rows rendered verbatim — the row shape `image_block_lines`
/// hands the frame builder (sequence + blank reservation rows).
struct RawRows(Vec<String>);

impl pir::tui::core::Component for RawRows {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }

    fn render_into(&self, _width: usize, out: &mut pir::tui::core::Rows<'_>) {
        for row in &self.0 {
            out.push(row);
        }
    }
}

struct ScrolledImageFixture {
    core: pir::tui::core::TuiCore,
    log_path: std::path::PathBuf,
    sequence: String,
    image_row: usize,
    transcript_end: usize,
    viewport_h: usize,
}

/// A scrolled-painter fixture: 30 text rows, then a 3-row Kitty image block,
/// then 10 text rows and a 1-row dock. The first render follows the end; the
/// test then scrolls and re-renders, capturing the byte stream via the write
/// log (the seam the TUI regression tests use).
fn scrolled_image_fixture(name: &str, image_id: u32) -> ScrolledImageFixture {
    use pir::tui::core::{Terminal, Text, TuiCore};
    use pir::tui::stdin_buffer::WriteLog;

    let sequence = kitty_encode(
        "QUJD",
        KittyEncodeOptions {
            columns: Some(3),
            rows: Some(3),
            image_id: Some(image_id),
            move_cursor: false,
        },
    );
    let dir = std::env::temp_dir().join(format!("pir-x1-crop-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log_path = dir.join("writes.log");
    let _ = std::fs::remove_file(&log_path);

    let terminal = Terminal::with_write_log(WriteLog::at(&log_path));
    terminal.set_size(40, 12);
    let mut core = TuiCore::new(terminal);
    core.set_dock_height(1);
    let above: String = (1..=30)
        .map(|index| format!("above-{index:02}\n"))
        .collect();
    let below: String = (1..=10)
        .map(|index| format!("below-{index:02}\n"))
        .collect();
    core.push_content(Box::new(Text::with_padding(above, 0, 0)));
    core.push_content(Box::new(RawRows(vec![
        sequence.clone(),
        String::new(),
        String::new(),
    ])));
    core.push_content(Box::new(Text::with_padding(below, 0, 0)));
    core.push_content(Box::new(Text::with_padding("DOCK", 0, 0)));
    core.render();

    let image_row = core
        .last_frame_lines()
        .iter()
        .position(|line| line.contains(&format!("i={image_id}")))
        .expect("the image line is in the first frame");
    let frame_len = core.frame_buffer_length();
    let transcript_end = frame_len - core.dock_height();
    let viewport_h = core.rows() - core.dock_height();
    ScrolledImageFixture {
        core,
        log_path,
        sequence,
        image_row,
        transcript_end,
        viewport_h,
    }
}

fn cleanup_fixture(fixture: &mut ScrolledImageFixture) {
    fixture.core.stop();
    if let Some(dir) = fixture.log_path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A partially scrolled image block re-paints with the crop controls (drift
/// row 16): the image line two rows above the window top crops to
/// `y=66,h=34,r=1` on the viewport's top row, exactly the upstream
/// `layout.ts:359-378` re-emit.
#[test]
fn scrolled_painter_repaints_a_partially_scrolled_image_with_the_crop_controls() {
    let _guard = crop_test_lock();
    const IMAGE_ID: u32 = 0x0C12_0107;
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: IMAGE_ID,
        columns: 3,
        rows: 3,
        width_px: 100,
        height_px: 100,
    });
    let mut fixture = scrolled_image_fixture("crop", IMAGE_ID);
    let first = std::fs::read_to_string(&fixture.log_path).expect("first paint");

    // Scroll so the image line sits exactly 2 rows above the window top:
    // hiddenRows=2, one visible row -> the upstream fixture's y=66,h=34,r=1.
    let start = fixture.image_row + 2;
    let offset = fixture.transcript_end - fixture.viewport_h - start;
    assert!(offset > 0, "the fixture must take the scrolled path");
    fixture.core.set_scroll_offset(offset);
    fixture.core.render();

    let stream = std::fs::read_to_string(&fixture.log_path).expect("scrolled paint");
    let scrolled = &stream[first.len()..];
    let expected = format!("\x1b_Ga=T,f=100,q=2,C=1,c=3,i={IMAGE_ID},y=66,h=34,r=1;QUJD\x1b\\");
    assert!(
        scrolled.contains(&expected),
        "the image line above the window re-paints as a crop at the viewport top: {scrolled:?}"
    );
    assert!(
        !scrolled.contains(&fixture.sequence),
        "the uncropped transmission must not be re-emitted: {scrolled:?}"
    );
    cleanup_fixture(&mut fixture);
}

/// A fully visible image block passes through the scrolled painter
/// byte-identical (the crop's `hiddenRows === 0 && croppedRows === rows`
/// no-op), so a frame that would not have been cropped before is unchanged.
#[test]
fn scrolled_painter_leaves_a_fully_visible_image_line_byte_identical() {
    let _guard = crop_test_lock();
    const IMAGE_ID: u32 = 0x0C12_0108;
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: IMAGE_ID,
        columns: 3,
        rows: 3,
        width_px: 100,
        height_px: 100,
    });
    let mut fixture = scrolled_image_fixture("visible", IMAGE_ID);
    let first = std::fs::read_to_string(&fixture.log_path).expect("first paint");

    // Three text rows above the block, eight below: the whole 3-row image is
    // inside the window.
    let start = fixture.image_row.saturating_sub(3);
    let offset = fixture.transcript_end - fixture.viewport_h - start;
    assert!(offset > 0, "the fixture must take the scrolled path");
    fixture.core.set_scroll_offset(offset);
    fixture.core.render();

    let stream = std::fs::read_to_string(&fixture.log_path).expect("scrolled paint");
    let scrolled = &stream[first.len()..];
    assert!(
        scrolled.contains(&fixture.sequence),
        "a fully visible image line is written verbatim: {scrolled:?}"
    );
    assert!(
        !scrolled.contains(",y="),
        "no crop controls are added when nothing is hidden: {scrolled:?}"
    );
    cleanup_fixture(&mut fixture);
}

/// An image line with no registered metadata stays untouched (the crop's
/// `!metadata` no-op): the window keeps its blank reservation rows and no
/// image bytes are painted at the top.
#[test]
fn scrolled_painter_leaves_an_unregistered_image_line_untouched() {
    let _guard = crop_test_lock();
    const IMAGE_ID: u32 = 0x0C12_01FF;
    let mut fixture = scrolled_image_fixture("unregistered", IMAGE_ID);
    let first = std::fs::read_to_string(&fixture.log_path).expect("first paint");

    let start = fixture.image_row + 2;
    let offset = fixture.transcript_end - fixture.viewport_h - start;
    assert!(offset > 0, "the fixture must take the scrolled path");
    fixture.core.set_scroll_offset(offset);
    fixture.core.render();

    let stream = std::fs::read_to_string(&fixture.log_path).expect("scrolled paint");
    let scrolled = &stream[first.len()..];
    assert!(
        !scrolled.contains(&format!("i={IMAGE_ID}")),
        "an unregistered image line is not re-painted: {scrolled:?}"
    );
    assert!(
        !scrolled.contains(",y="),
        "no crop controls without registered metadata: {scrolled:?}"
    );
    cleanup_fixture(&mut fixture);
}
