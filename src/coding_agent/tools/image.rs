//! Image processing for the read tool.
//!
//! Rust port of upstream `utils/mime.ts`, `utils/image-process.ts`, and
//! `utils/image-resize-core.ts`: sniff a supported inline-image MIME type from
//! a file head, normalize to a supported format (BMP re-encodes to PNG), and
//! resize to fit the model's inline image limits, trying PNG and JPEG
//! encodings and progressively shrinking until the base64 payload fits.

// This module has two halves:
//
// * the MIME sniffer (`IMAGE_TYPE_SNIFF_BYTES` + `detect_supported_image_mime_type`
//   below) is a crate-free byte-signature check and always compiles — the
//   `read` tool's image branch and `print.rs`'s `@file` sniff both rely on it;
// * the decode/resize/encode half (`processImage` at the bottom) needs the
//   `image` crate, so every item there carries
//   `#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]`:
//   `image-tools` enables read's inline images plus print's attachment path;
//   print keeps its (`tui`-backed) attachment path without `image-tools`.
//   `tui` alone does not compile these helpers: the TUI's own `image` crate
//   use (preview/settings) lives in `src/tui`, and read attachment is strictly
//   the `image-tools` feature (docs/FEATURES.md §5).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
use base64::Engine as _;

/// How many leading bytes the MIME sniffer may inspect (upstream
/// `IMAGE_TYPE_SNIFF_BYTES`).
pub(crate) const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

/// Upstream `ImageResizeOptions` defaults: 2000x2000, 4.5MB of base64 payload
/// (below Anthropic's 5MB request limit), JPEG quality 80.
///
/// Part of the decode/resize path below, so — like that path — these exist
/// only when the `image` crate does.
#[cfg(feature = "image-tools")]
pub(crate) const DEFAULT_MAX_WIDTH: u32 = 2000;
#[cfg(feature = "image-tools")]
pub(crate) const DEFAULT_MAX_HEIGHT: u32 = 2000;
#[cfg(feature = "image-tools")]
pub(crate) const DEFAULT_MAX_BYTES: usize = 4_500_000;
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
pub(crate) const DEFAULT_JPEG_QUALITY: u8 = 80;

/// Hard per-side decode limit (a decompression-bomb guard, far above any
/// legitimate inline image: the resize path shrinks to 2000 px anyway).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
const IMAGE_DECODE_MAX_DIMENSION: u32 = 16_384;

/// Hard cap on one decoded image allocation. `image` defaults to 512 MiB;
/// state it explicitly so the bound does not drift with the dependency.
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
const IMAGE_DECODE_MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

// ---------------------------------------------------------------------------
// MIME sniffing (upstream mime.ts)
// ---------------------------------------------------------------------------

/// Detect a supported inline image MIME type from a file head (upstream
/// `detectSupportedImageMimeType`). `None` for non-images and for malformed or
/// animated variants upstream refuses (animated PNG, progressivized JPEG).
pub(crate) fn detect_supported_image_mime_type(head: &[u8]) -> Option<&'static str> {
    if starts_with(head, &[0xff, 0xd8, 0xff]) {
        return if head.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(head, &PNG_SIGNATURE) {
        return if is_png(head) && !is_animated_png(head) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(head, 0, "GIF87a") || starts_with_ascii(head, 0, "GIF89a") {
        return Some("image/gif");
    }
    if starts_with_ascii(head, 0, "RIFF") && starts_with_ascii(head, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(head, 0, "BM") && is_bmp(head) {
        return Some("image/bmp");
    }
    None
}

fn is_png(head: &[u8]) -> bool {
    head.len() >= 16
        && read_u32_be(head, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(head, 12, "IHDR")
}

fn is_animated_png(head: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= head.len() {
        let chunk_length = read_u32_be(head, offset) as usize;
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(head, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(head, chunk_type_offset, "IDAT") {
            return false;
        }
        let Some(next_offset) = offset
            .checked_add(8)
            .and_then(|value| value.checked_add(chunk_length))
            .and_then(|value| value.checked_add(4))
        else {
            return false;
        };
        if next_offset <= offset || next_offset > head.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

fn is_bmp(head: &[u8]) -> bool {
    if head.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(head, 2);
    let pixel_data_offset = read_u32_le(head, 10);
    let dib_header_size = read_u32_le(head, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if (pixel_data_offset as u64) < 14 + u64::from(dib_header_size) {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }
    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        if head.len() < 26 {
            return false;
        }
        (read_u16_le(head, 22), read_u16_le(head, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if head.len() < 30 {
            return false;
        }
        (read_u16_le(head, 26), read_u16_le(head, 28))
    } else {
        return false;
    };
    color_planes == 1 && matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

fn read_u16_le(head: &[u8], offset: usize) -> u32 {
    let low = u32::from(head.get(offset).copied().unwrap_or(0));
    let high = u32::from(head.get(offset + 1).copied().unwrap_or(0));
    low | (high << 8)
}

fn read_u32_be(head: &[u8], offset: usize) -> u32 {
    let mut value = 0u32;
    for index in 0..4 {
        let byte = u32::from(head.get(offset + index).copied().unwrap_or(0));
        value = (value << 8) | byte;
    }
    value
}

fn read_u32_le(head: &[u8], offset: usize) -> u32 {
    let mut value = 0u32;
    for index in (0..4).rev() {
        let byte = u32::from(head.get(offset + index).copied().unwrap_or(0));
        value = (value << 8) | byte;
    }
    value
}

fn starts_with(head: &[u8], prefix: &[u8]) -> bool {
    head.len() >= prefix.len() && head[..prefix.len()] == *prefix
}

fn starts_with_ascii(head: &[u8], offset: usize, text: &str) -> bool {
    let bytes = text.as_bytes();
    head.len() >= offset + bytes.len() && head[offset..offset + bytes.len()] == *bytes
}

// ---------------------------------------------------------------------------
// processImage (upstream image-process.ts + image-resize-core.ts)
// ---------------------------------------------------------------------------
//
// Everything below decodes/encodes pixels and therefore needs the `image`
// crate; each item is gated on "the crate exists" (see the module note at the
// top). Without the gate only the crate-free sniffer above remains.

/// A processed inline image: base64 data URL payload plus the resize hints
/// that accompany it (upstream `ProcessImageResult` ok shape).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
pub(crate) struct ProcessedImage {
    pub data_base64: String,
    pub mime_type: String,
    pub hints: Vec<String>,
}

/// Why `processImage` produced no inline image (upstream
/// `ProcessImageResult` failure shape carries the note text).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessImageError {
    /// The bytes could not be normalized to a supported inline format.
    Convert,
    /// No PNG/JPEG candidate fit the inline byte limit.
    Resize,
}

#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
impl ProcessImageError {
    /// The note appended to the text block (upstream `processImage` messages).
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Convert => {
                "[Image omitted: could not be converted to a supported inline image format.]"
            }
            Self::Resize => {
                "[Image omitted: could not be resized below the inline image size limit.]"
            }
        }
    }
}

/// Decode image bytes, applying the file's EXIF orientation first (upstream
/// `applyExifOrientation` in `resizeImageInProcess` and `encodePng`, which
/// rotates/flips the decoded pixels before any resize). `None` when the bytes
/// cannot be decoded.
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn decode_image(bytes: &[u8]) -> Option<image::DynamicImage> {
    use image::ImageDecoder as _;

    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    // Reject decompression bombs before the decoder allocates the pixel
    // buffer: the declared dimensions and the cumulative decoded allocation
    // are both bounded (an image with a huge IHDR no longer allocates).
    // `Limits` is `#[non_exhaustive]`, so fill a default value.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(IMAGE_DECODE_MAX_DIMENSION);
    limits.max_image_height = Some(IMAGE_DECODE_MAX_DIMENSION);
    limits.max_alloc = Some(IMAGE_DECODE_MAX_ALLOC_BYTES);
    decoder.set_limits(limits).ok()?;
    // Decoders for formats without orientation metadata report NoTransforms;
    // a decoder that cannot report it is treated the same way.
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded = image::DynamicImage::from_decoder(decoder).ok()?;
    decoded.apply_orientation(orientation);
    Some(decoded)
}

/// Resize an image to fit within the max dimensions and encoded size. Returns
/// the error variant that upstream reports when the image cannot be decoded
/// (Convert) or no candidate fits `max_bytes` (Resize).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
pub(crate) fn process_image(
    bytes: &[u8],
    mime_type: &str,
    max_width: u32,
    max_height: u32,
    max_bytes: usize,
    jpeg_quality: u8,
) -> Result<ProcessedImage, ProcessImageError> {
    let (normalized, normalized_mime, converted_from) =
        normalize_image(bytes, mime_type).ok_or(ProcessImageError::Convert)?;

    // Encoded payload size if passed through unchanged (base64 of the bytes).
    let input_base64_size = encoded_base64_size(normalized.len());

    // Decode once for dimension checks and any resize (EXIF orientation
    // applied by `decode_image`).
    let decoded = decode_image(&normalized).ok_or(ProcessImageError::Convert)?;

    let (original_width, original_height) = (decoded.width(), decoded.height());

    // Already within the limits: pass the (normalized) bytes through unchanged.
    if original_width <= max_width && original_height <= max_height && input_base64_size < max_bytes
    {
        return Ok(ProcessedImage {
            data_base64: base64::engine::general_purpose::STANDARD.encode(&normalized),
            mime_type: normalized_mime.clone(),
            hints: conversion_hint(converted_from, &normalized_mime),
        });
    }

    // Initial target dimensions respecting the max limits (upstream rule:
    // `Math.round((targetHeight * maxWidth) / targetWidth)`).
    let mut target_width = original_width;
    let mut target_height = original_height;
    if target_width > max_width {
        target_height = scale_round(target_height, max_width, target_width);
        target_width = max_width;
    }
    if target_height > max_height {
        target_width = scale_round(target_width, max_height, target_height);
        target_height = max_height;
    }

    // Quality steps: the configured quality plus upstream's ladder, deduped
    // in insertion order (`Array.from(new Set([opts.jpegQuality, 85, 70, 55, 40]))`
    // keeps the first occurrence, so the configured quality leads the ladder),
    // and candidates are tried in order (PNG first, then the JPEG ladder).
    let mut qualities = vec![jpeg_quality, 85, 70, 55, 40];
    let mut seen = std::collections::HashSet::new();
    qualities.retain(|quality| seen.insert(*quality));

    let mut current_width = target_width;
    let mut current_height = target_height;
    loop {
        let resized = decoded.resize(
            current_width,
            current_height,
            image::imageops::FilterType::Lanczos3,
        );
        let mut candidates: Vec<(String, String)> = Vec::new();
        if let Some(png) = encode_png_candidate(&resized) {
            candidates.push(png);
        }
        for quality in &qualities {
            if let Some(jpeg) = encode_jpeg_candidate(&resized, *quality) {
                candidates.push(jpeg);
            }
        }
        if let Some((data_base64, mime)) = candidates
            .into_iter()
            .find(|(data, _)| data.len() < max_bytes)
        {
            let mut hints = conversion_hint(converted_from, &mime);
            hints.extend(format_dimension_note(
                original_width,
                original_height,
                current_width,
                current_height,
            ));
            return Ok(ProcessedImage {
                data_base64,
                mime_type: mime,
                hints,
            });
        }

        if current_width == 1 && current_height == 1 {
            return Err(ProcessImageError::Resize);
        }
        let next_width = (current_width * 3 / 4).max(1);
        let next_height = (current_height * 3 / 4).max(1);
        if next_width == current_width && next_height == current_height {
            return Err(ProcessImageError::Resize);
        }
        current_width = next_width;
        current_height = next_height;
    }
}

/// `Math.round(value * factor / divisor)` with the saturating arithmetic the
/// resize ladder needs (`Math.round` rounds half away from zero; both factors
/// are positive).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn scale_round(value: u32, factor: u32, divisor: u32) -> u32 {
    if divisor == 0 {
        return value;
    }
    let scaled = u64::from(value) * u64::from(factor);
    let rounded = (scaled + u64::from(divisor) / 2) / u64::from(divisor);
    u32::try_from(rounded.max(1)).unwrap_or(u32::MAX)
}

/// Normalize to a supported inline format: png/jpeg/gif/webp pass through
/// unchanged (aliases like `image/jpg` canonicalize); anything else that can
/// be decoded (upstream BMP and friends) re-encodes to PNG. Returns
/// `(bytes, normalized mime, original base mime)` on success.
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn normalize_image(bytes: &[u8], mime_type: &str) -> Option<(Vec<u8>, String, Option<String>)> {
    let base = base_mime_type(mime_type);
    match base.as_str() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => {
            Some((bytes.to_vec(), base, None))
        }
        _ => {
            // Upstream convertImageBytesToPng: decode with the image library
            // (EXIF orientation applied) and re-encode as PNG.
            let decoded = decode_image(bytes)?;
            let encoded = encode_png_vec(&decoded)?;
            Some((encoded, "image/png".to_owned(), Some(base)))
        }
    }
}

/// Strip parameters and lowercase (upstream `baseMimeType`).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn conversion_hint(from: Option<String>, to: &str) -> Vec<String> {
    match from {
        Some(from) if from != to => vec![format!("[Image converted from {from} to {to}.]")],
        _ => Vec::new(),
    }
}

/// Upstream `formatDimensionNote`: helps the model map coordinates back.
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn format_dimension_note(
    original_width: u32,
    original_height: u32,
    width: u32,
    height: u32,
) -> Option<String> {
    if width == 0 {
        return None;
    }
    let scale = f64::from(original_width) / f64::from(width);
    Some(format!(
        "[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {scale:.2} to map to original image.]"
    ))
}

/// Size of the base64 encoding of `len` bytes (upstream computes the same
/// ceiling without materializing the string).
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn encoded_base64_size(len: usize) -> usize {
    len.div_ceil(3) * 4
}

#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn encode_png_vec(image: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    image.write_to(&mut buffer, image::ImageFormat::Png).ok()?;
    Some(buffer.into_inner())
}

/// Upstream `encodeCandidate(resized.get_bytes(), "image/png")`: the PNG
/// candidate is always offered first.
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn encode_png_candidate(image: &image::DynamicImage) -> Option<(String, String)> {
    let bytes = encode_png_vec(image)?;
    Some((
        base64::engine::general_purpose::STANDARD.encode(bytes),
        "image/png".to_owned(),
    ))
}

/// Upstream `encodeCandidate(resized.get_bytes_jpeg(quality), "image/jpeg")`.
#[cfg(any(feature = "image-tools", all(feature = "print", feature = "tui")))]
fn encode_jpeg_candidate(image: &image::DynamicImage, quality: u8) -> Option<(String, String)> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
    let rgb = image.to_rgb8();
    let (width, height) = (rgb.width(), rgb.height());
    let mut encoder = encoder;
    encoder
        .encode(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some((
        base64::engine::general_purpose::STANDARD.encode(buffer.get_ref()),
        "image/jpeg".to_owned(),
    ))
}
