//! Pins the read-tool image pipeline fix (upstream `image-resize-core.ts`):
//! the JPEG quality ladder keeps insertion order
//! (`Array.from(new Set([opts.jpegQuality, 85, 70, 55, 40]))` keeps the first
//! occurrence), so the configured quality is the first JPEG candidate — the
//! previous sorted ladder (`qualities.sort_unstable()`) always started at 40
//! for any image that needed a resize.
//!
//! `src/coding_agent/tools/image.rs` is pulled in verbatim via `#[path]`
//! (its items are `pub(crate)`, unreachable through the public `pir` API;
//! the module has no crate-internal imports, so it compiles standalone).

#[path = "../src/coding_agent/tools/image.rs"]
// The module's callers live in the pir lib, which this test crate does not
// include, so some of its items are dead here by construction.
#[allow(dead_code)]
mod image_impl;

use std::io::Cursor;

use base64::Engine as _;

/// Deterministic pseudo-random noise: PNG barely compresses it while the
/// JPEG candidates stay comfortably smaller, so the byte limits below force
/// the PNG candidate to fail and let the quality ladder decide.
fn noise_png(width: u32, height: u32) -> Vec<u8> {
    let mut state = 0x243f_6a88_85a3_08d3_u64;
    let mut raw = Vec::with_capacity((width * height * 3) as usize);
    for _ in 0..width * height {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        raw.push((state >> 33) as u8);
        raw.push((state >> 41) as u8);
        raw.push((state >> 49) as u8);
    }
    let source = ::image::RgbImage::from_raw(width, height, raw).expect("raw image");
    let mut png = Vec::new();
    ::image::DynamicImage::ImageRgb8(source)
        .write_to(&mut Cursor::new(&mut png), ::image::ImageFormat::Png)
        .expect("png encode");
    png
}

fn decode(bytes: &[u8]) -> ::image::DynamicImage {
    ::image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .expect("reader")
        .decode()
        .expect("decode")
}

/// `process_image` resizes a 256x200 source to exactly 64x50 for the 64x64
/// limits (`200 * 64 / 256 == 50`) with Lanczos3.
fn resized_to_target(image: &::image::DynamicImage) -> ::image::DynamicImage {
    image.resize(64, 50, ::image::imageops::FilterType::Lanczos3)
}

fn png_base64(image: &::image::DynamicImage) -> String {
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), ::image::ImageFormat::Png)
        .expect("png encode");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn jpeg_base64(image: &::image::DynamicImage, quality: u8) -> String {
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
    let rgb = image.to_rgb8();
    let (width, height) = (rgb.width(), rgb.height());
    encoder
        .encode(
            rgb.as_raw(),
            width,
            height,
            ::image::ExtendedColorType::Rgb8,
        )
        .expect("jpeg encode");
    base64::engine::general_purpose::STANDARD.encode(buffer.get_ref())
}

#[test]
fn configured_default_quality_is_the_first_jpeg_candidate() {
    // The defaults the read tool falls back to when the model defines no
    // `inputLimits.images.resize`.
    assert_eq!(
        (
            image_impl::DEFAULT_MAX_WIDTH,
            image_impl::DEFAULT_MAX_HEIGHT,
            image_impl::DEFAULT_MAX_BYTES,
            image_impl::DEFAULT_JPEG_QUALITY,
        ),
        (2000, 2000, 4_500_000, 80)
    );

    let png = noise_png(256, 200);
    let head = &png[..png.len().min(image_impl::IMAGE_TYPE_SNIFF_BYTES)];
    assert_eq!(
        image_impl::detect_supported_image_mime_type(head),
        Some("image/png")
    );

    let resized = resized_to_target(&decode(&png));
    let png_candidate = png_base64(&resized);
    let quality_80 = jpeg_base64(&resized, 80);
    let quality_40 = jpeg_base64(&resized, 40);
    // Fixture sanity: the PNG candidate must exceed the limit (so the JPEG
    // ladder decides) and the qualities must encode differently.
    assert!(png_candidate.len() > quality_80.len());
    assert_ne!(quality_80, quality_40);

    // The limit equals the PNG candidate's size, so the strict
    // `data.len() < max_bytes` check rejects PNG and the first fitting JPEG
    // wins.
    let processed = image_impl::process_image(
        &png,
        "image/png",
        64,
        64,
        png_candidate.len(),
        image_impl::DEFAULT_JPEG_QUALITY,
    )
    .expect("processed");

    assert_eq!(processed.mime_type, "image/jpeg");
    assert_eq!(
        processed.data_base64, quality_80,
        "the configured default quality (80) must lead the ladder; a sorted ladder encodes at 40"
    );
    assert_ne!(processed.data_base64, quality_40);
    assert!(
        processed
            .hints
            .iter()
            .any(|hint| hint.contains("displayed at 64x50")),
        "hints: {:?}",
        processed.hints
    );
}

#[test]
fn quality_ladder_dedupes_in_insertion_order() {
    let png = noise_png(256, 200);
    let resized = resized_to_target(&decode(&png));
    let png_candidate = png_base64(&resized);
    let quality_85 = jpeg_base64(&resized, 85);
    let quality_70 = jpeg_base64(&resized, 70);
    assert!(png_candidate.len() > quality_85.len());
    assert!(quality_70.len() < quality_85.len());

    // `jpeg_quality = 85` dedupes to [85, 70, 55, 40] in insertion order.
    // The limit equals the 85 encoding's size, so 85 fails and the next
    // insertion-order step (70) wins; a sorted ladder would fall to 40.
    let processed = image_impl::process_image(&png, "image/png", 64, 64, quality_85.len(), 85)
        .expect("processed");

    assert_eq!(processed.data_base64, quality_70);
    assert_ne!(processed.data_base64, jpeg_base64(&resized, 40));
}

/// P1 regression: an image whose declared width exceeds the explicit decode
/// dimension limit is rejected before the decoder allocates its pixel buffer
/// (a PNG bomb), instead of decoding and resizing normally.
#[test]
fn decode_rejects_images_over_the_dimension_limit() {
    // One pixel above IMAGE_DECODE_MAX_DIMENSION (16_384); a 20_000x1 RGB
    // image is tiny to encode but would decode without the limit.
    let png = noise_png(20_000, 1);
    let result = image_impl::process_image(&png, "image/png", 2000, 2000, 4_500_000, 80);
    assert!(
        matches!(result, Err(image_impl::ProcessImageError::Convert)),
        "an over-limit image must be rejected at decode"
    );
}
