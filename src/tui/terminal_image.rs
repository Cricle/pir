//! Terminal capability detection — port of upstream
//! `packages/tui/src/terminal-image.ts` (`TerminalCapabilities`,
//! `detectCapabilitiesFromEnvironment`, `detectCapabilities`,
//! `getTerminalColorMode`) plus the settings bridge upstream keeps in
//! `settings-manager.ts` (`getTerminalCapabilityOverrides`).
//!
//! Consumers: the settings menu's image rows, which upstream gates on
//! `getCapabilities().images` ("Only show image toggle if terminal supports
//! it", `settings-selector.ts:759-776`), and the `terminal.images` /
//! `PI_IMAGE_PROTOCOL` override resolution. pir's Rust TUI has no inline
//! image renderer yet (the halfblocks preview in [`super::image_preview`]
//! needs only truecolor), so this is the pi-tui parity surface the renderer
//! will read when it lands.
//!
//! DRIFT D21 (upstream `78a3c10ba`, #10774): Herdr forwards OSC 8 hyperlinks
//! *and* supports the Kitty graphics protocol, so a `TERM_PROGRAM=herdr`
//! session reports `images: kitty`. Herdr must be checked before the outer
//! terminal's detection because variables such as `KITTY_WINDOW_ID` leak into
//! its panes.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::config::settings::{AutoBool, TerminalImagesMode, TerminalSettings};

/// The Kitty graphics protocol escape prefix (upstream `KITTY_PREFIX`).
pub const KITTY_PREFIX: &str = "\x1b_G";
/// The iTerm2 inline-image escape prefix (upstream `ITERM2_PREFIX`).
pub const ITERM2_PREFIX: &str = "\x1b]1337;File=";

/// Default terminal cell size in pixels (upstream `cellDimensions` default,
/// updated when the terminal answers the cell-size query).
pub const DEFAULT_CELL_WIDTH_PX: f64 = 9.0;
pub const DEFAULT_CELL_HEIGHT_PX: f64 = 18.0;

/// Kitty protocol chunk size (upstream `CHUNK_SIZE`).
const KITTY_CHUNK_SIZE: usize = 4096;

/// Inline image protocol a terminal can render (upstream `ImageProtocol`;
/// `null` there is [`Option::None`] here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

/// Detected terminal capabilities (upstream `TerminalCapabilities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// The inline image protocol, or `None` when the terminal has none.
    pub images: Option<ImageProtocol>,
    /// Whether the terminal renders 24-bit color.
    pub true_color: bool,
    /// Whether the terminal renders OSC 8 hyperlinks.
    pub hyperlinks: bool,
}

/// Partial capability overrides (upstream `Partial<TerminalCapabilities>`).
/// The outer `Option` means "an override is present"; `images:
/// Some(None)` forces images *off* (upstream `images: null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilityOverrides {
    pub images: Option<Option<ImageProtocol>>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
}

/// `PI_HYPERLINKS` / `PI_TRUE_COLOR` parsing: only the exact strings `"1"`
/// and `"0"` are overrides; anything else (including `"auto"`) keeps the
/// detected value (upstream `parseBooleanCapabilityOverride`).
fn parse_boolean_override(value: Option<&str>) -> Option<bool> {
    match value {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// Lower-cased environment read (upstream compares lower-cased values).
fn env_lower(env: &dyn Fn(&str) -> Option<String>, name: &str) -> String {
    env(name).unwrap_or_default().to_lowercase()
}

fn env_present(env: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    env(name).is_some_and(|value| !value.is_empty())
}

/// The environment half of the detection (upstream
/// `detectCapabilitiesFromEnvironment`), with `tmux_forwards_hyperlink` as
/// the tmux OSC 8 probe. Branch order is upstream's, including the Herdr
/// branch before the outer-terminal checks (DRIFT D21).
pub fn detect_from_environment(
    env: &dyn Fn(&str) -> Option<String>,
    tmux_forwards_hyperlink: &dyn Fn() -> bool,
) -> TerminalCapabilities {
    let term_program = env_lower(env, "TERM_PROGRAM");
    let terminal_emulator = env_lower(env, "TERMINAL_EMULATOR");
    let term = env_lower(env, "TERM");
    let color_term = env_lower(env, "COLORTERM");
    let has_true_color_hint =
        color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct");
    let is_windows_console = cfg!(target_os = "windows");

    let images = |protocol: Option<ImageProtocol>, true_color: bool, hyperlinks: bool| {
        TerminalCapabilities {
            images: protocol,
            true_color,
            hyperlinks,
        }
    };

    // tmux only re-emits OSC 8 hyperlinks when its client_termfeatures say so
    // and image protocols are unreliable through it.
    if env_present(env, "TMUX") || term.starts_with("tmux") {
        return images(None, has_true_color_hint, tmux_forwards_hyperlink());
    }
    // screen does not forward OSC 8 hyperlinks.
    if term.starts_with("screen") {
        return images(None, has_true_color_hint, false);
    }
    // D21: Herdr forwards OSC 8 hyperlinks and supports the Kitty graphics
    // protocol; check it before outer-terminal detection because variables
    // like KITTY_WINDOW_ID can leak into its panes.
    if term_program == "herdr" {
        return images(Some(ImageProtocol::Kitty), has_true_color_hint, true);
    }
    if env_present(env, "KITTY_WINDOW_ID") || term_program == "kitty" {
        return images(Some(ImageProtocol::Kitty), true, true);
    }
    if term_program == "ghostty"
        || term.contains("ghostty")
        || env_present(env, "GHOSTTY_RESOURCES_DIR")
    {
        return images(Some(ImageProtocol::Kitty), true, true);
    }
    if env_present(env, "WEZTERM_PANE") || term_program == "wezterm" {
        return images(Some(ImageProtocol::Kitty), true, true);
    }
    // Warp supports the Kitty graphics protocol and OSC 8 hyperlinks.
    if term_program == "warpterminal"
        || env_present(env, "WARP_SESSION_ID")
        || env_present(env, "WARP_TERMINAL_SESSION_UUID")
    {
        return images(Some(ImageProtocol::Kitty), true, true);
    }
    if env_present(env, "ITERM_SESSION_ID") || term_program == "iterm.app" {
        return images(Some(ImageProtocol::Iterm2), true, true);
    }
    if env_present(env, "WT_SESSION") {
        return images(None, true, true);
    }
    if term_program == "alacritty" || term_program == "vscode" || term_program == "zed" {
        return images(None, true, true);
    }
    if terminal_emulator == "jetbrains-jediterm" {
        return images(None, true, false);
    }
    // Windows Terminal does not always set WT_SESSION (a cmd.exe launched
    // from Win+R); modern consoles support truecolor, hyperlinks stay off
    // unless positively detected above.
    if is_windows_console {
        return images(None, true, false);
    }
    // Unknown terminal: be conservative — OSC 8 renders invisibly as "just
    // text" where it is not supported, so `text (url)` is the default.
    images(None, has_true_color_hint, false)
}

/// Detection plus the environment overrides `PI_HYPERLINKS`,
/// `PI_IMAGE_PROTOCOL` (`kitty`/`iterm2`, or `none`/`0` to disable) and
/// `PI_TRUE_COLOR` (upstream `detectCapabilities`).
pub fn detect_capabilities_with(
    env: &dyn Fn(&str) -> Option<String>,
    tmux_forwards_hyperlink: &dyn Fn() -> bool,
) -> TerminalCapabilities {
    let hyperlinks = parse_boolean_override(env("PI_HYPERLINKS").as_deref());
    // An explicit `PI_HYPERLINKS` override also bypasses the tmux probe.
    let mut detected = match hyperlinks {
        Some(value) => detect_from_environment(env, &|| value),
        None => detect_from_environment(env, tmux_forwards_hyperlink),
    };
    let image_protocol = env("PI_IMAGE_PROTOCOL").map(|value| value.to_lowercase());
    match image_protocol.as_deref() {
        Some("kitty") => detected.images = Some(ImageProtocol::Kitty),
        Some("iterm2") => detected.images = Some(ImageProtocol::Iterm2),
        Some("none") | Some("0") => detected.images = None,
        _ => {}
    }
    if let Some(true_color) = parse_boolean_override(env("PI_TRUE_COLOR").as_deref()) {
        detected.true_color = true_color;
    }
    if let Some(hyperlinks) = hyperlinks {
        detected.hyperlinks = hyperlinks;
    }
    detected
}

/// [`detect_capabilities_with`] over the process environment.
///
/// Deviation: upstream's default tmux probe runs `tmux display-message -p
/// '#{client_termfeatures}'`; pir does not spawn a child process on this path
/// and reports `false`, the conservative answer upstream falls back to on any
/// probe error (the same choice the extension runtime's JS shim makes). Only
/// tmux sessions are affected, and only for the hyperlink flag.
pub fn detect_capabilities() -> TerminalCapabilities {
    detect_capabilities_with(&process_env, &|| false)
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Apply partial overrides on top of a detected set (upstream
/// `setCapabilityOverrides`'s merge in `getCapabilities`).
pub fn apply_overrides(capabilities: &mut TerminalCapabilities, overrides: CapabilityOverrides) {
    if let Some(images) = overrides.images {
        capabilities.images = images;
    }
    if let Some(true_color) = overrides.true_color {
        capabilities.true_color = true_color;
    }
    if let Some(hyperlinks) = overrides.hyperlinks {
        capabilities.hyperlinks = hyperlinks;
    }
}

/// Detection with `overrides` applied.
pub fn capabilities_with_overrides(overrides: CapabilityOverrides) -> TerminalCapabilities {
    let mut capabilities = detect_capabilities();
    apply_overrides(&mut capabilities, overrides);
    capabilities
}

/// The capability overrides the `terminal` settings carry (upstream
/// `SettingsManager.getTerminalCapabilityOverrides`): `images` as
/// `"kitty"`/`"iterm2"`/`false`, `trueColor` and `hyperlinks` as booleans
/// (`"auto"` keeps the detected value).
pub fn overrides_from_terminal_settings(
    terminal: Option<&TerminalSettings>,
) -> CapabilityOverrides {
    let Some(terminal) = terminal else {
        return CapabilityOverrides::default();
    };
    CapabilityOverrides {
        images: match terminal.images {
            Some(TerminalImagesMode::Kitty) => Some(Some(ImageProtocol::Kitty)),
            Some(TerminalImagesMode::Iterm2) => Some(Some(ImageProtocol::Iterm2)),
            Some(TerminalImagesMode::Off) => Some(None),
            Some(TerminalImagesMode::Auto) | None => None,
        },
        true_color: terminal.true_color.and_then(auto_bool_value),
        hyperlinks: terminal.hyperlinks.and_then(auto_bool_value),
    }
}

fn auto_bool_value(value: AutoBool) -> Option<bool> {
    match value {
        AutoBool::Enabled(enabled) => Some(enabled),
        AutoBool::Auto => None,
    }
}

/// Whether the terminal can render inline images at all — upstream
/// `SettingsSelectorComponent`'s `supportsImages` gate for the "Show images"
/// and "Image width" rows (`settings-selector.ts:471, 759-776`).
pub fn supports_images(terminal: Option<&TerminalSettings>) -> bool {
    capabilities_with_overrides(overrides_from_terminal_settings(terminal))
        .images
        .is_some()
}

/// The inline-image protocol the `terminal` settings resolve to (settings
/// override when present, otherwise detection). `None` = text fallback.
pub fn protocol_from_settings(terminal: Option<&TerminalSettings>) -> Option<ImageProtocol> {
    capabilities_with_overrides(overrides_from_terminal_settings(terminal)).images
}

// ---------------------------------------------------------------------------
// Protocol output (upstream `terminal-image.ts` encoders + `renderImage`)
// ---------------------------------------------------------------------------

/// Process-wide resolved capabilities. The TUI installs the settings-resolved
/// set at startup (and when the image settings change); a build/test that
/// never installs one falls back to environment detection.
static CAPABILITIES: std::sync::OnceLock<std::sync::Mutex<Option<TerminalCapabilities>>> =
    std::sync::OnceLock::new();

/// Monotonic counter bumped whenever the resolved capability set is installed.
/// Renderers memoizing image rows key on it so a live `terminal.images`
/// change cannot leave rows encoded for the previous protocol.
static CAPABILITIES_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn capabilities_cell() -> &'static std::sync::Mutex<Option<TerminalCapabilities>> {
    CAPABILITIES.get_or_init(|| std::sync::Mutex::new(None))
}

/// The current capability generation (see [`CAPABILITIES_GENERATION`]).
pub fn capabilities_generation() -> u64 {
    CAPABILITIES_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Install the resolved capability set (upstream `setCapabilities`).
pub fn set_resolved_capabilities(capabilities: TerminalCapabilities) {
    *capabilities_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(capabilities);
    CAPABILITIES_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The installed capability set, or a fresh environment detection when none
/// was installed.
pub fn resolved_capabilities() -> TerminalCapabilities {
    capabilities_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or_else(detect_capabilities)
}

/// Whether a rendered line carries an inline image escape sequence (upstream
/// `isImageLine`): the fast path checks the line start, the slow path any
/// position (multi-row images prefix cursor-up movements).
pub fn is_image_line(line: &str) -> bool {
    line.starts_with(KITTY_PREFIX)
        || line.starts_with(ITERM2_PREFIX)
        || line.contains(KITTY_PREFIX)
        || line.contains(ITERM2_PREFIX)
}

/// Allocate a Kitty image id (upstream `allocateImageId`: a random value in
/// `[1, 0xfffffffe]`). pir mixes the clock and a counter instead of pulling
/// an RNG dependency.
pub fn allocate_image_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let mixed = counter
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(nanos.wrapping_mul(0x85EB_CA6B))
        ^ std::process::id();
    (mixed % 0xFFFF_FFFE).wrapping_add(1)
}

/// One Kitty image placement (upstream `encodeKitty` options).
#[derive(Debug, Clone, Copy, Default)]
pub struct KittyEncodeOptions {
    pub columns: Option<usize>,
    pub rows: Option<usize>,
    pub image_id: Option<u32>,
    /// `false` adds `C=1` (Kitty must not move the cursor).
    pub move_cursor: bool,
}

/// Encode one image as Kitty graphics protocol sequences (upstream
/// `encodeKitty`): `a=T,f=100,q=2`, optional `c`/`r`/`i`, `C=1` when
/// `move_cursor` is false, and 4096-byte base64 chunks with
/// `m=1`/`m=0` continuation flags. The no-cursor-movement default in
/// [`KittyEncodeOptions::default`] matches the transcript use
/// (`moveCursor: false`).
pub fn encode_kitty(base64_data: &str, options: KittyEncodeOptions) -> String {
    let mut params = vec!["a=T".to_string(), "f=100".to_string(), "q=2".to_string()];
    if !options.move_cursor {
        params.push("C=1".to_string());
    }
    if let Some(columns) = options.columns.filter(|columns| *columns > 0) {
        params.push(format!("c={columns}"));
    }
    if let Some(rows) = options.rows.filter(|rows| *rows > 0) {
        params.push(format!("r={rows}"));
    }
    if let Some(image_id) = options.image_id {
        params.push(format!("i={image_id}"));
    }
    let params = params.join(",");
    if base64_data.len() <= KITTY_CHUNK_SIZE {
        return format!("\x1b_G{params};{base64_data}\x1b\\");
    }
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut first = true;
    while offset < base64_data.len() {
        let end = (offset + KITTY_CHUNK_SIZE).min(base64_data.len());
        let chunk = &base64_data[offset..end];
        let last = end >= base64_data.len();
        if first {
            chunks.push(format!("\x1b_G{params},m=1;{chunk}\x1b\\"));
            first = false;
        } else if last {
            chunks.push(format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            chunks.push(format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset = end;
    }
    chunks.concat()
}

/// One iTerm2 inline image (upstream `encodeITerm2` options).
#[derive(Debug, Clone, Default)]
pub struct Iterm2EncodeOptions {
    /// Cell count, or `"auto"` (upstream accepts both).
    pub width: Option<String>,
    pub height: Option<String>,
    pub name: Option<String>,
    pub preserve_aspect_ratio: bool,
}

/// Encode one image as an iTerm2 inline-image sequence (upstream
/// `encodeITerm2`): `inline=1;size=<bytes>;<width>;<height>;<name>:<base64>`
/// terminated by BEL. `size` is the decoded byte length.
pub fn encode_iterm2(base64_data: &str, options: &Iterm2EncodeOptions) -> String {
    // Base64 is 4 chars -> 3 bytes; count the padding so `size=` is exact.
    let padding = base64_data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    let size = (base64_data.len() / 4 * 3).saturating_sub(padding);
    let mut params = vec!["inline=1".to_string(), format!("size={size}")];
    if let Some(width) = &options.width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = &options.height {
        params.push(format!("height={height}"));
    }
    if let Some(name) = &options.name {
        use base64::Engine;
        let name = base64::engine::general_purpose::STANDARD.encode(name);
        params.push(format!("name={name}"));
    }
    if !options.preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\x1b]1337;File={}:{}\x07", params.join(";"), base64_data)
}

/// One rendered image placement (upstream `renderImage` result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage {
    pub sequence: String,
    /// Reserved cells across.
    pub columns: usize,
    /// Reserved rows down.
    pub rows: usize,
    pub image_id: Option<u32>,
}

/// Cell footprint of an image (upstream `calculateImageCellSize`), including
/// the Kitty even-cell aspect optimization.
pub fn calculate_image_cell_size(
    width_px: u32,
    height_px: u32,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
    optimize_aspect_ratio: bool,
) -> (usize, usize) {
    let max_width = max_width_cells.max(1) as f64;
    let image_width = width_px.max(1) as f64;
    let image_height = height_px.max(1) as f64;
    let width_scale = (max_width * DEFAULT_CELL_WIDTH_PX) / image_width;
    let height_scale = match max_height_cells {
        Some(max_height) => (max_height.max(1) as f64 * DEFAULT_CELL_HEIGHT_PX) / image_height,
        None => width_scale,
    };
    let scale = width_scale.min(height_scale);
    let scaled_width = image_width * scale;
    let scaled_height = image_height * scale;
    let columns = (scaled_width / DEFAULT_CELL_WIDTH_PX)
        .ceil()
        .max(1.0)
        .min(max_width) as usize;
    let mut rows = (scaled_height / DEFAULT_CELL_HEIGHT_PX).ceil().max(1.0) as usize;
    if let Some(max_height) = max_height_cells {
        rows = rows.min(max_height.max(1));
    }
    if optimize_aspect_ratio {
        // Kitty: choose the even row count closer to the ideal aspect so the
        // cell-aligned image is less distorted (upstream
        // `chooseLessDistortedCellCount`). iTerm2 keeps its reservation.
        let ideal_rows = (columns as f64 * DEFAULT_CELL_WIDTH_PX * image_height)
            / (image_width * DEFAULT_CELL_HEIGHT_PX);
        rows = choose_less_distorted_cell_count(rows, ideal_rows);
    }
    (columns, rows)
}

/// The nearer of `current` and its neighbours `±1`, skipping zero
/// (upstream `chooseLessDistortedCellCount`).
fn choose_less_distorted_cell_count(current: usize, ideal: f64) -> usize {
    let candidates = [
        current.saturating_sub(1),
        current,
        current.saturating_add(1),
    ];
    candidates
        .into_iter()
        .filter(|candidate| *candidate > 0)
        .min_by(|a, b| {
            let da = (*a as f64 - ideal).abs();
            let db = (*b as f64 - ideal).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(current.max(1))
}

/// Whether a protocol can display the given MIME type as-is. Kitty's
/// graphics protocol only accepts PNG: upstream transcodes other formats
/// (which needs the image pipeline). iTerm2 accepts the four image types the
/// content blocks carry.
pub fn protocol_supports_mime(protocol: ImageProtocol, mime_type: &str) -> bool {
    match protocol {
        ImageProtocol::Kitty => mime_type.eq_ignore_ascii_case("image/png"),
        ImageProtocol::Iterm2 => matches!(
            mime_type.to_ascii_lowercase().as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ),
    }
}

/// The image types the built-in PNG transcoder can convert for Kitty
/// (upstream `ensurePngTranscoder` / Photon's decoders: JPEG, GIF, WebP).
pub fn transcodable_mime(mime_type: &str) -> bool {
    matches!(
        mime_type.to_ascii_lowercase().as_str(),
        "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Whether `protocol` can display `mime_type`, including the built-in
/// non-PNG → PNG transcode Kitty needs. The image crate is gated behind
/// `image-protocol`; a slim build keeps the as-is check only and renders the
/// text fallback for every other format.
pub fn protocol_can_display_mime(protocol: ImageProtocol, mime_type: &str) -> bool {
    protocol_supports_mime(protocol, mime_type)
        || (cfg!(feature = "image-protocol")
            && protocol == ImageProtocol::Kitty
            && transcodable_mime(mime_type))
}

/// One cached transcode: the payload key and its PNG base64 result.
type PngCacheEntry = (u64, Option<String>);

/// Cache of transcoded PNG base64 payloads, keyed by a hash of the source
/// payload + MIME (upstream `Image`'s `pngCache`, bounded to 32 entries).
/// A failed conversion is cached as `None`: sending the same broken bytes to
/// the decoder on every frame would repeat the failure at frame rate.
fn png_cache() -> &'static std::sync::Mutex<Vec<PngCacheEntry>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Vec<PngCacheEntry>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn png_cache_key(base64_data: &str, mime_type: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    mime_type.hash(&mut hasher);
    base64_data.hash(&mut hasher);
    hasher.finish()
}

/// Decode `base64_data` and re-encode it as PNG base64 for Kitty (upstream
/// `convertToPng`). `None` when the bytes do not decode as an image or the
/// encoder fails; the caller renders the text fallback.
///
/// The file's EXIF orientation is applied before encoding (upstream Photon's
/// decoders honor it, and the image tools' `resizeImageInProcess` applies the
/// same transform): a phone JPEG with `Orientation=6` must reach the terminal
/// upright, not sideways.
#[cfg(feature = "image-protocol")]
fn transcode_to_png_base64(base64_data: &str, _mime_type: &str) -> Option<String> {
    // Bounded decode: an attachment larger than this cannot be previewed
    // sensibly and would only spike memory before failing.
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    use base64::Engine;
    use image::ImageDecoder as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()?;
    if decoded.is_empty() || decoded.len() > MAX_BYTES {
        return None;
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(&decoded))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    // Formats without orientation metadata report `NoTransforms`; a decoder
    // that cannot report it is treated the same way.
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(&png))
}

#[cfg(not(feature = "image-protocol"))]
fn transcode_to_png_base64(_base64_data: &str, _mime_type: &str) -> Option<String> {
    None
}

/// [`transcode_to_png_base64`] behind the process-wide cache.
fn cached_png_base64(base64_data: &str, mime_type: &str) -> Option<String> {
    let key = png_cache_key(base64_data, mime_type);
    {
        let cache = png_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, cached)) = cache.iter().find(|(cached_key, _)| *cached_key == key) {
            return cached.clone();
        }
    }
    let png = transcode_to_png_base64(base64_data, mime_type);
    let mut cache = png_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.insert(0, (key, png.clone()));
    cache.truncate(32);
    png
}

/// Render one base64 image for a protocol (upstream `renderImage`); `None`
/// when the protocol has no support (or the dimensions are unusable).
pub fn render_image(
    base64_data: &str,
    width_px: u32,
    height_px: u32,
    protocol: ImageProtocol,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
) -> Option<RenderedImage> {
    if base64_data.is_empty() || width_px == 0 || height_px == 0 {
        return None;
    }
    match protocol {
        ImageProtocol::Kitty => {
            let (columns, rows) = calculate_image_cell_size(
                width_px,
                height_px,
                max_width_cells,
                max_height_cells,
                true,
            );
            let image_id = allocate_image_id();
            let sequence = encode_kitty(
                base64_data,
                KittyEncodeOptions {
                    columns: Some(columns),
                    rows: Some(rows),
                    image_id: Some(image_id),
                    move_cursor: false,
                },
            );
            Some(RenderedImage {
                sequence,
                columns,
                rows,
                image_id: Some(image_id),
            })
        }
        ImageProtocol::Iterm2 => {
            let (columns, rows) = calculate_image_cell_size(
                width_px,
                height_px,
                max_width_cells,
                max_height_cells,
                false,
            );
            let sequence = encode_iterm2(
                base64_data,
                &Iterm2EncodeOptions {
                    width: Some(columns.to_string()),
                    height: Some("auto".to_string()),
                    name: None,
                    preserve_aspect_ratio: true,
                },
            );
            Some(RenderedImage {
                sequence,
                columns,
                rows,
                image_id: None,
            })
        }
    }
}

/// The transcript lines for one image block, in the upstream
/// `components/image.ts` shape:
///
/// - Kitty: the sequence on the first line, followed by `rows - 1` empty
///   reservations (`C=1` means the terminal does not move the cursor).
/// - iTerm2: `rows - 1` empty reservations, then a cursor-up prefix plus the
///   sequence on the last row so cursor accounting stays inside the scroll
///   area.
///
/// `None` when the image cannot be placed (the caller renders the upstream
/// `[Image: …]` fallback).
pub fn image_block_lines(
    base64_data: &str,
    width_px: u32,
    height_px: u32,
    protocol: ImageProtocol,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
) -> Option<Vec<String>> {
    let rendered = render_image(
        base64_data,
        width_px,
        height_px,
        protocol,
        max_width_cells,
        max_height_cells,
    )?;
    let mut lines = Vec::with_capacity(rendered.rows);
    match protocol {
        ImageProtocol::Kitty => {
            lines.push(rendered.sequence);
            for _ in 1..rendered.rows {
                lines.push(String::new());
            }
        }
        ImageProtocol::Iterm2 => {
            for _ in 1..rendered.rows {
                lines.push(String::new());
            }
            let offset = rendered.rows.saturating_sub(1);
            let move_up = if offset > 0 {
                format!("\x1b[{offset}A")
            } else {
                String::new()
            };
            lines.push(format!("{move_up}{}", rendered.sequence));
        }
    }
    Some(lines)
}

/// Render one base64 image content block at `max_width_cells` (an assistant
/// or tool-result `ContentBlock::Image`). Falls back to the upstream
/// `[Image: <mime> <WxH>]` text line when the protocol cannot place it, the
/// dimensions cannot be sniffed, or (Kitty) a non-PNG payload cannot be
/// transcoded.
pub fn image_content_lines(
    base64_data: &str,
    mime_type: &str,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
) -> Vec<String> {
    image_content_lines_with_protocol(
        resolved_capabilities().images,
        base64_data,
        mime_type,
        max_width_cells,
        max_height_cells,
    )
}

/// [`image_content_lines`] with an explicit protocol (test seam and callers
/// that resolve capabilities outside the process-wide cell).
///
/// As-is formats go straight to the encoder. On Kitty a JPEG/GIF/WebP block
/// is transcoded to PNG first (the protocol only accepts PNG); a conversion
/// failure degrades to the same text fallback as a missing protocol.
pub fn image_content_lines_with_protocol(
    protocol: Option<ImageProtocol>,
    base64_data: &str,
    mime_type: &str,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
) -> Vec<String> {
    if let Some(protocol) = protocol {
        if protocol_supports_mime(protocol, mime_type) {
            if let Some((width, height)) = decode_base64_prefix(base64_data)
                .as_deref()
                .and_then(crate::tui::image_preview::sniff_image_dimensions)
            {
                if let Some(lines) = image_block_lines(
                    base64_data,
                    width,
                    height,
                    protocol,
                    max_width_cells,
                    max_height_cells,
                ) {
                    return lines;
                }
            }
        } else if protocol == ImageProtocol::Kitty && transcodable_mime(mime_type) {
            if let Some(png) = cached_png_base64(base64_data, mime_type) {
                if let Some((width, height)) = decode_base64_prefix(&png)
                    .as_deref()
                    .and_then(crate::tui::image_preview::sniff_image_dimensions)
                {
                    if let Some(lines) = image_block_lines(
                        &png,
                        width,
                        height,
                        protocol,
                        max_width_cells,
                        max_height_cells,
                    ) {
                        return lines;
                    }
                }
            }
        }
    }
    let dimensions = decode_base64_prefix(base64_data)
        .as_deref()
        .and_then(crate::tui::image_preview::sniff_image_dimensions);
    let mut fallback = format!("[Image: {mime_type}");
    if let Some((width, height)) = dimensions {
        fallback.push_str(&format!(" {width}x{height}"));
    }
    fallback.push(']');
    vec![fallback]
}

/// Decode the leading bytes of a base64 image (only as much as the header
/// sniff needs — decoding a multi-MB attachment would copy it every frame).
fn decode_base64_prefix(base64_data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    const MAX_BYTES: usize = 4096;
    // 4 base64 chars -> 3 bytes; clamp to a multiple of 4 so padding rules
    // never reject the prefix.
    let chars = (MAX_BYTES / 3 + 1) * 4;
    let end = base64_data
        .char_indices()
        .nth(chars)
        .map(|(index, _)| index)
        .unwrap_or(base64_data.len());
    // Whole 4-char groups only: a partial group is not valid base64.
    let end = end - (end % 4);
    let prefix = &base64_data[..end];
    base64::engine::general_purpose::STANDARD
        .decode(prefix)
        .ok()
}
