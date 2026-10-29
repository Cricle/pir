//! Image preview for the TUI.
//!
//! The **fallback + settings** half (`image_fallback_line`,
//! `sniff_image_dimensions`, `image_width_cells`, `show_images`,
//! `ImageSettings`, [`render_image_with_settings`]) always compiles: it backs
//! the `terminal.showImages` / `terminal.imageWidthCells` consumers, which are
//! live in a default build (the `/image` preview and the tool-result image
//! text fallback both read them). The **halfblocks renderer**
//! ([`render_image_halfblocks`]) draws a local image as ANSI lines using the
//! Unicode upper half-block (`▀`): each terminal cell shows two vertically
//! adjacent pixels — top pixel as the cell's foreground, bottom pixel as its
//! background. Truecolor is always used for previews (they only make sense
//! in image-capable terminals).
//!
//! The renderer is gated by the `image-protocol` cargo feature
//! (`image-protocol = ["tui"]`). It is part of `full`, so the default build
//! keeps the preview; a slim TUI build can turn it off independently, and
//! then `terminal.showImages: true` renders the text fallback below instead
//! of halfblocks (same read/error behavior).
//!
//! Fallback (upstream `pi-tui` `terminal-image.ts` `imageFallback` +
//! `coding-agent` `render-utils.ts`): when the image cannot be rendered
//! (unreadable, undecodable, oversized), a styled text line
//! `[Image: <~/path> [<mime>] <WxH>]` is produced instead of an error, with
//! the dimensions sniffed from the PNG/JPEG/GIF/WEBP headers (upstream
//! `getImageDimensions`). pir renders the fallback as plain text — the
//! theme-token seam (`theme.fg(...)`) is not available in this module and
//! inline ANSI would fight the transcript renderer's line styling.
//!
//! This is a deliberately small substitute for the `ratatui-image` crate:
//! that crate's recent versions do support ratatui 0.30, but integrating it
//! requires rendering through a full `ratatui::Terminal`/buffer bridge which
//! pir's own line-based differential renderer does not have, and it pulls in
//! sixel/Kitty protocol backends plus several non-optional dependencies. The
//! halfblocks renderer achieves the visible goal (preview local image files
//! in the transcript) with none of that integration cost.

use std::path::Path;

/// Render `path` as at most `max_width` columns × `max_height` rows of
/// half-block ANSI lines. Falls back to the upstream-style
/// `[Image: ...]` text line when the file cannot be read or decoded
/// (dimensions sniffed from the headers when possible); never panics.
/// Errors only when there is no area to render into at all.
///
/// The `image` crate rides the `tui` feature (`dep:image`). This renderer is
/// the `image-protocol` half of the TUI: in `full` (default), and omitted
/// when a slim TUI build turns the feature off — [`render_image_with_settings`]
/// then renders the text fallback below.
#[cfg(feature = "image-protocol")]
pub fn render_image_halfblocks(
    path: &Path,
    max_width: usize,
    max_height: usize,
) -> Result<Vec<String>, String> {
    if max_width == 0 || max_height == 0 {
        return Err("preview area is empty".to_string());
    }
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            return Ok(vec![image_fallback_line(path, None, &error.to_string())]);
        }
    };
    // Bounded decode: refuse absurd inputs before handing them to the decoder.
    const MAX_BYTES: u64 = 32 * 1024 * 1024;
    if data.len() as u64 > MAX_BYTES {
        return Ok(vec![image_fallback_line(
            path,
            Some(&data),
            "image file is too large to preview",
        )]);
    }
    let image = match image::load_from_memory(&data) {
        Ok(image) => image,
        Err(error) => {
            return Ok(vec![image_fallback_line(
                path,
                Some(&data),
                &error.to_string(),
            )]);
        }
    };

    // Each cell is 1×2 pixels; fit inside the preview box.
    let (img_w, img_h) = (image.width().max(1), image.height().max(1));
    let max_px_w = max_width as u32;
    let max_px_h = (max_height.saturating_mul(2)).max(2) as u32;
    let scale_w = max_px_w as f64 / img_w as f64;
    let scale_h = max_px_h as f64 / img_h as f64;
    let scale = scale_w.min(scale_h).min(1.0);
    let cols = ((img_w as f64 * scale).floor() as u32).max(1).min(max_px_w);
    let rows_px = ((img_h as f64 * scale).floor() as u32).max(2).min(max_px_h);
    let rows_px = rows_px - (rows_px % 2);

    let thumbnail = image::imageops::thumbnail(&image, cols, rows_px);
    let mut lines = Vec::with_capacity((rows_px / 2) as usize);
    for row in 0..rows_px / 2 {
        let mut line = String::new();
        for col in 0..cols {
            let top = thumbnail.get_pixel(col, row * 2);
            let bottom = thumbnail.get_pixel(col, row * 2 + 1);
            line.push_str(&format!(
                "\x1b[38;2;{};{};{};48;2;{};{};{}m▀",
                top[0], top[1], top[2], bottom[0], bottom[1], bottom[2]
            ));
        }
        line.push_str("\x1b[0m");
        lines.push(line);
    }
    Ok(lines)
}

// ---------------------------------------------------------------------------
// Text fallback (upstream `imageFallback` + `getImageDimensions`)
// ---------------------------------------------------------------------------

/// Sniff image dimensions `(width, height)` from the raw file headers, for
/// the PNG/JPEG/GIF/WEBP formats upstream supports
/// (`terminal-image.ts getImageDimensions`); `None` for anything else or a
/// truncated header. Public for tests/.
pub fn sniff_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(data)
        .or_else(|| jpeg_dimensions(data))
        .or_else(|| gif_dimensions(data))
        .or_else(|| webp_dimensions(data))
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || data[..4] != [0x89, b'P', b'N', b'G'] {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((width, height))
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut offset = 2usize;
    while offset < data.len().saturating_sub(9) {
        if data[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = data.get(offset + 1).copied()?;
        if (0xC0..=0xC2).contains(&marker) {
            let height =
                u16::from_be_bytes(data.get(offset + 5..offset + 7)?.try_into().ok()?) as u32;
            let width =
                u16::from_be_bytes(data.get(offset + 7..offset + 9)?.try_into().ok()?) as u32;
            return Some((width, height));
        }
        if offset + 3 >= data.len() {
            return None;
        }
        let length =
            u16::from_be_bytes(data.get(offset + 2..offset + 4)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        offset += 2 + length;
    }
    None
}

fn gif_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 {
        return None;
    }
    if &data[..6] != b"GIF87a" && &data[..6] != b"GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes(data[6..8].try_into().ok()?) as u32;
    let height = u16::from_le_bytes(data[8..10].try_into().ok()?) as u32;
    Some((width, height))
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 30 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }
    match &data[12..16] {
        b"VP8 " => {
            let width = u16::from_le_bytes(data[26..28].try_into().ok()?) as u32 & 0x3FFF;
            let height = u16::from_le_bytes(data[28..30].try_into().ok()?) as u32 & 0x3FFF;
            Some((width, height))
        }
        b"VP8L" => {
            let bits = u32::from_le_bytes(data[21..25].try_into().ok()?);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            Some((width, height))
        }
        b"VP8X" => {
            let width =
                (u32::from(data[24]) | u32::from(data[25]) << 8 | u32::from(data[26]) << 16) + 1;
            let height =
                (u32::from(data[27]) | u32::from(data[28]) << 8 | u32::from(data[29]) << 16) + 1;
            Some((width, height))
        }
        _ => None,
    }
}

/// The image MIME type for a fallback line: the file extension first (the
/// upstream caller supplies the mime from the content block; a filesystem
/// path maps through its extension), then the sniffed magic bytes, else
/// upstream's `image/unknown`.
fn image_mime(path: &Path, data: Option<&[u8]>) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => return "image/png".to_string(),
        Some("jpg" | "jpeg") => return "image/jpeg".to_string(),
        Some("gif") => return "image/gif".to_string(),
        Some("webp") => return "image/webp".to_string(),
        _ => {}
    }
    let Some(data) = data else {
        return "image/unknown".to_string();
    };
    if png_dimensions(data).is_some() {
        "image/png".to_string()
    } else if jpeg_dimensions(data).is_some() {
        "image/jpeg".to_string()
    } else if gif_dimensions(data).is_some() {
        "image/gif".to_string()
    } else if webp_dimensions(data).is_some() {
        "image/webp".to_string()
    } else {
        "image/unknown".to_string()
    }
}

/// Shorten home-prefixed absolute paths to `~/...` for compact display
/// (upstream `shortenImagePath`).
fn shorten_image_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    let Some(home) = crate::session_manager::home_dir() else {
        return text;
    };
    let home = home.to_string_lossy().into_owned();
    if !home.is_empty()
        && (text == home
            || text.starts_with(&format!("{home}/"))
            || text.starts_with(&format!("{home}\\")))
    {
        return format!("~{}", &text[home.len()..]);
    }
    text
}

/// The upstream `[Image: <~/path> [<mime>] <WxH>]` fallback line
/// (`terminal-image.ts imageFallback`, parts joined in its order: filename,
/// `[mime]`, dimensions). `detail` carries the failure reason after the
/// dimensions (pir addition: upstream never reaches the fallback with a
/// failure, pir's /image path falls back on one).
pub fn image_fallback_line(path: &Path, data: Option<&[u8]>, detail: &str) -> String {
    let mime = image_mime(path, data);
    let dimensions = data.and_then(sniff_image_dimensions);
    let mut parts = vec![shorten_image_path(path), format!("[{mime}]")];
    if let Some((width, height)) = dimensions {
        parts.push(format!("{width}x{height}"));
    }
    if !detail.is_empty() {
        parts.push(format!("({detail})"));
    }
    format!("[Image: {}]", parts.join(" "))
}

/// `terminal.imageWidthCells` as the preview width: upstream
/// `settingsManager.getImageWidthCells()` (default 60, clamped to at least
/// one cell). The stored value is already schema-bounded, so this only
/// guards against a hand-edited `0`.
pub fn image_width_cells(setting: Option<u32>) -> usize {
    setting.unwrap_or(DEFAULT_IMAGE_WIDTH_CELLS).max(1) as usize
}

/// The upstream default for `terminal.imageWidthCells`
/// (`SETTINGS_DEFAULTS.terminal.imageWidthCells`).
pub const DEFAULT_IMAGE_WIDTH_CELLS: u32 = 60;

/// `terminal.showImages` (default `true`, upstream
/// `getShowImages()`): whether inline image output is rendered. `false`
/// replaces every preview with the upstream text fallback line
/// (`render-utils.ts getTextOutput`: images render only when the terminal
/// supports them AND `showImages` is on).
pub fn show_images(setting: Option<bool>) -> bool {
    setting.unwrap_or(true)
}

/// The two `terminal.*` image settings, resolved (W11-C): the preview width in
/// cells (`terminal.imageWidthCells`, default 60) and whether inline images
/// render at all (`terminal.showImages`, default `true`). Upstream reads both
/// through `settingsManager.getImageWidthCells()` / `getShowImages()` at every
/// render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSettings {
    pub width_cells: usize,
    pub show: bool,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            width_cells: image_width_cells(None),
            show: show_images(None),
        }
    }
}

impl ImageSettings {
    /// Resolve from a settings `terminal` object.
    pub fn from_terminal(terminal: Option<&crate::config::settings::TerminalSettings>) -> Self {
        Self {
            width_cells: image_width_cells(
                terminal.and_then(|terminal| terminal.image_width_cells),
            ),
            show: show_images(terminal.and_then(|terminal| terminal.show_images)),
        }
    }
}

/// Render `path` honoring `settings`: the width comes from
/// `terminal.imageWidthCells`. With `show = true` the `image-protocol`
/// feature renders halfblocks; without the feature (or with `show = false`)
/// the text fallback is produced instead (upstream `getTextOutput(result,
/// showImages)` / `imageFallback`), with the same read/error behavior either
/// way. `max_height` is the caller's row budget.
pub fn render_image_with_settings(
    path: &Path,
    max_height: usize,
    settings: ImageSettings,
) -> Result<Vec<String>, String> {
    #[cfg(feature = "image-protocol")]
    if settings.show {
        return render_image_halfblocks(path, settings.width_cells, max_height);
    }
    // Without the renderer the width/height budgets are unused: the fallback
    // is a single text line.
    #[cfg(not(feature = "image-protocol"))]
    let _ = (settings, max_height);
    // Hidden (or no renderer compiled in): the text fallback, with the
    // upstream failure reason when the file cannot be read (the same
    // contract as an undecodable image).
    match std::fs::read(path) {
        Ok(data) => Ok(vec![image_fallback_line(path, Some(&data), "")]),
        Err(error) => Ok(vec![image_fallback_line(path, None, &error.to_string())]),
    }
}

/// [`render_image_with_settings`] with the inline protocol preferred: when
/// the terminal settings resolve to Kitty or iTerm2, the file is emitted as
/// protocol output through the shared content-block renderer (Kitty gets a
/// PNG transcode for JPEG/GIF/WebP, upstream `renderImage` +
/// `components/image.ts`). Everything else (no protocol, an unencodable
/// payload, an unreadable file) falls back to the halfblocks renderer or the
/// upstream text line exactly as [`render_image_with_settings`] does.
pub fn render_image_with_terminal(
    path: &Path,
    max_height: usize,
    settings: ImageSettings,
    terminal: Option<&crate::config::settings::TerminalSettings>,
) -> Result<Vec<String>, String> {
    if settings.show {
        if let Some(protocol) = crate::tui::terminal_image::protocol_from_settings(terminal) {
            if let Ok(data) = std::fs::read(path) {
                let mime = image_mime(path, Some(&data));
                use base64::Engine;
                let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
                // Shared with assistant/tool-result content blocks: Kitty gets
                // a PNG transcode for JPEG/GIF/WebP instead of falling back to
                // halfblocks for every non-PNG file.
                let lines = crate::tui::terminal_image::image_content_lines_with_protocol(
                    Some(protocol),
                    &encoded,
                    &mime,
                    settings.width_cells,
                    Some(max_height),
                );
                if lines
                    .iter()
                    .any(|line| crate::tui::terminal_image::is_image_line(line))
                {
                    return Ok(lines);
                }
            }
        }
    }
    render_image_with_settings(path, max_height, settings)
}
