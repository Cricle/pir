//! Performance guard for the interactive `@file` image path: a 12MB-class
//! high-entropy 2000x2000 PNG must reach the 2000x2000 / 4.5MB profile
//! quickly instead of encoding every full-size candidate first.
//!
//! The fixture is assembled as a PNG with zlib "stored" blocks (no
//! compression), so generating 12MB of noise is a memcpy plus CRC/Adler
//! passes rather than a slow debug-build deflate.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use pir::coding_agent::context::{expand_prompt_file_args_with_options, ImageAttachmentOptions};

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    (value >> 1) ^ 0xEDB8_8320
                } else {
                    value >> 1
                };
            }
            *slot = value;
        }
        table
    })
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u64);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += u64::from(a);
        }
        a %= 65521;
        b %= 65521;
    }
    ((b as u32) << 16) | a
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

/// zlib stream made of uncompressed deflate blocks (BFINAL on the last one).
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = raw.chunks(u16::MAX as usize).peekable();
    while let Some(chunk) = chunks.next() {
        out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn png_from_raw(width: u32, height: u32, color_type: u8, raw: &[u8]) -> Vec<u8> {
    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, color_type, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib_stored(raw));
    png_chunk(&mut png, b"IEND", &[]);
    png
}

/// 2000x2000 RGB noise: roughly 12MB because stored deflate cannot shrink it.
fn noise_rgb_png(width: u32, height: u32) -> Vec<u8> {
    let row_len = width as usize * 3;
    let stride = row_len + 1;
    let mut raw = vec![0u8; stride * height as usize];
    let mut state = 0x243F_6A88_85A3_08D3u64;
    for y in 0..height as usize {
        raw[y * stride] = 0; // filter type: None
        for pixel in raw[y * stride + 1..(y + 1) * stride].chunks_exact_mut(3) {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            pixel[0] = (value >> 24) as u8;
            pixel[1] = (value >> 40) as u8;
            pixel[2] = (value >> 56) as u8;
        }
    }
    png_from_raw(width, height, 2, &raw)
}

/// RGBA gradient with a varying alpha channel; exceeds max width so the
/// resize path (not pass-through) must preserve the alpha channel.
fn alpha_gradient_png(width: u32, height: u32) -> Vec<u8> {
    let row_len = width as usize * 4;
    let stride = row_len + 1;
    let mut raw = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        raw[y * stride] = 0;
        for (x, pixel) in raw[y * stride + 1..(y + 1) * stride]
            .chunks_exact_mut(4)
            .enumerate()
        {
            let x = x as u32;
            pixel[0] = (x % 256) as u8;
            pixel[1] = ((x * 3) % 256) as u8;
            pixel[2] = ((x * 7) % 256) as u8;
            pixel[3] = ((x + y as u32) % 256) as u8;
        }
    }
    png_from_raw(width, height, 6, &raw)
}

fn small_rgb_png(width: u32, height: u32) -> Vec<u8> {
    let row_len = width as usize * 3;
    let stride = row_len + 1;
    let mut raw = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        let row = y * stride;
        raw[row] = 0;
        for (x, pixel) in raw[row + 1..row + 1 + row_len]
            .chunks_exact_mut(3)
            .enumerate()
        {
            let x = x as u32;
            let y = y as u32;
            pixel[0] = (x % 256) as u8;
            pixel[1] = ((x * 3 + y) % 256) as u8;
            pixel[2] = ((x * 7 + y * 5) % 256) as u8;
        }
    }
    png_from_raw(width, height, 2, &raw)
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-image-resize-perf-{name}-{stamp}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn options() -> ImageAttachmentOptions {
    ImageAttachmentOptions {
        max_width: 2000,
        max_height: 2000,
        max_bytes: 4_500_000,
        jpeg_quality: 80,
    }
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("image data must be valid base64")
}

/// `(width, height)` from a PNG IHDR or a JPEG SOF marker.
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

#[test]
fn twelve_megabyte_noise_png_resizes_within_limits_and_time_budget() {
    let dir = temp_dir("noise");
    let source = noise_rgb_png(2000, 2000);
    assert!(
        source.len() >= 11_000_000,
        "fixture must be 12MB-class, got {} bytes",
        source.len()
    );
    write_file(&dir, "noise.png", &source);

    let started = Instant::now();
    let (text, images) = expand_prompt_file_args_with_options("@noise.png", &dir, Some(options()))
        .expect("expand @noise.png");
    let elapsed = started.elapsed();

    assert_eq!(images.len(), 1, "one image attachment");
    assert!(
        text.contains("noise.png"),
        "image marker line kept, got {text:?}"
    );
    let decoded = base64_decode(&images[0].data);
    let (width, height) = image_dimensions(&decoded);
    eprintln!(
        "12MB noise PNG: expansion {elapsed:?}, output {width}x{height} {}, base64 {} bytes",
        images[0].mime_type,
        images[0].data.len()
    );
    assert!(
        width <= 2000 && height <= 2000,
        "output must fit 2000x2000, got {width}x{height}"
    );
    assert!(
        images[0].data.len() < 4_500_000,
        "base64 payload must stay under 4.5MB, got {}",
        images[0].data.len()
    );
    assert_eq!(
        images[0].mime_type, "image/jpeg",
        "large opaque input should take the JPEG fast path"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "12MB noise PNG expansion took {elapsed:?}, budget is 10s"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transparent_png_keeps_alpha_after_resize() {
    let dir = temp_dir("alpha");
    let source = alpha_gradient_png(2100, 64);
    write_file(&dir, "alpha.png", &source);

    let (_, images) = expand_prompt_file_args_with_options("@alpha.png", &dir, Some(options()))
        .expect("expand @alpha.png");

    assert_eq!(images.len(), 1);
    assert_eq!(
        images[0].mime_type, "image/png",
        "an alpha PNG must stay PNG (JPEG would drop the alpha channel)"
    );
    let decoded = base64_decode(&images[0].data);
    let (width, height) = image_dimensions(&decoded);
    assert!(
        width <= 2000 && height <= 2000,
        "resized alpha PNG must fit the limits, got {width}x{height}"
    );
    let image = ::image::load_from_memory(&decoded).expect("decode resized alpha PNG");
    assert!(
        image.color().has_alpha(),
        "resized alpha PNG lost its alpha channel (color type {:?})",
        image.color()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn small_image_passes_through_byte_for_byte() {
    let dir = temp_dir("small");
    let source = small_rgb_png(64, 64);
    write_file(&dir, "small.png", &source);

    let (_, images) = expand_prompt_file_args_with_options("@small.png", &dir, Some(options()))
        .expect("expand @small.png");

    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(
        base64_decode(&images[0].data),
        source,
        "an image already within the limits must pass through unchanged"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
