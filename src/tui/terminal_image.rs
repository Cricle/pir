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
use std::sync::Arc;

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
    // Rex supports OSC 8 hyperlinks and enough of the Kitty graphics protocol
    // for Pi (upstream 396ecd7b8 / 0ed1b0431). Check it before the outer
    // terminal checks so a leaked KITTY_WINDOW_ID cannot change the answer.
    if term_program == "rex" {
        return images(Some(ImageProtocol::Kitty), true, true);
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

/// Whether `lines` is exactly the block shape [`image_block_lines`] emits: one
/// complete Kitty/iTerm2 image sequence plus blank reservation rows.
///
/// The TUI uses this to tell a trusted `/image` preview (`ChatEntry::Raw` with
/// no renderer provenance) apart from untrusted raw text (extension renderer
/// output, custom-entry JSON). The check is structural and deliberately
/// strict: exactly ONE non-empty line, starting at the line start (iTerm2 may
/// carry the cursor-up CSI prefix), with a complete terminator and printable
/// payload. A line that merely embeds a prefix, is truncated, or carries any
/// other control byte does not qualify — it stays on the text sanitizer.
pub fn is_image_protocol_lines(lines: &[String]) -> bool {
    let mut image_lines = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if !is_complete_image_sequence(line) {
            return false;
        }
        image_lines += 1;
    }
    image_lines == 1
}

fn is_complete_image_sequence(line: &str) -> bool {
    is_complete_kitty_sequence(line) || is_complete_iterm2_sequence(line)
}

/// One or more concatenated Kitty APC chunks (`ESC _ params ; base64 ESC \`),
/// as produced by [`encode_kitty`]. Requires the transmission command
/// (`a=T`) so control commands (`a=d`, ...) never qualify.
fn is_complete_kitty_sequence(line: &str) -> bool {
    if !line.starts_with(KITTY_PREFIX) || !line.ends_with("\x1b\\") {
        return false;
    }
    let mut rest = line;
    let mut first = true;
    while !rest.is_empty() {
        let Some(after_prefix) = rest.strip_prefix(KITTY_PREFIX) else {
            return false;
        };
        let Some(end) = after_prefix.find("\x1b\\") else {
            return false;
        };
        let chunk = &after_prefix[..end];
        let Some((params, payload)) = chunk.split_once(';') else {
            return false;
        };
        if payload.is_empty()
            || !payload
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
            || !chunk.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return false;
        }
        if first && !params.split(',').any(|param| param == "a=T") {
            return false;
        }
        first = false;
        rest = &after_prefix[end + 2..];
    }
    true
}

/// One iTerm2 inline image (`OSC 1337 ; File = … BEL`), optionally prefixed by
/// the cursor-up CSI [`image_block_lines`] adds so the placement stays inside
/// the scroll area.
fn is_complete_iterm2_sequence(line: &str) -> bool {
    let line = match line.strip_prefix("\x1b[") {
        Some(rest) => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || rest.as_bytes().get(digits) != Some(&b'A') {
                return false;
            }
            &rest[digits + 1..]
        }
        None => line,
    };
    let Some(body) = line.strip_prefix(ITERM2_PREFIX) else {
        return false;
    };
    let Some(payload) = body.strip_suffix('\x07') else {
        return false;
    };
    !payload.is_empty() && payload.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
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

// ---------------------------------------------------------------------------
// Kitty metadata registry + crop (upstream `terminal-image.ts`
// `registerKittyImageMetadata` / `getMaxKittyImageRows` /
// `getKittyImageMetadata` / `cropKittyImageLine`)
// ---------------------------------------------------------------------------

/// One registered Kitty image's cell footprint and pixel dimensions
/// (upstream `KittyImageMetadata`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyImageMetadata {
    pub image_id: u32,
    pub columns: usize,
    pub rows: usize,
    pub width_px: u32,
    pub height_px: u32,
}

/// Registry entry (upstream `RegisteredKittyImageMetadata`): the public
/// metadata plus the transmission generation the placement dedup keys on.
#[derive(Debug, Clone, Copy)]
struct RegisteredKittyImageMetadata {
    metadata: KittyImageMetadata,
    transmission_generation: u64,
}

/// Process-wide registration table (upstream `kittyImageMetadata`).
#[derive(Default)]
struct KittyMetadataRegistry {
    /// Registration order (upstream `Map` insertion order): a re-registration
    /// deletes the old entry and appends the new one, so it moves to the end.
    entries: Vec<(u32, RegisteredKittyImageMetadata)>,
    /// Tallest registered image (upstream `maxKittyImageRows`; monotonic).
    max_rows: usize,
    /// Bumped per registration (upstream `kittyTransmissionGeneration`).
    generation: u64,
}

/// Entry bound of the registry (upstream `kittyImageMetadata.size > 1000`).
const MAX_KITTY_IMAGE_METADATA: usize = 1000;

fn kitty_metadata_registry() -> &'static std::sync::Mutex<KittyMetadataRegistry> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<KittyMetadataRegistry>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(KittyMetadataRegistry::default()))
}

/// Register one rendered Kitty image (upstream `registerKittyImageMetadata`).
/// [`render_image`] calls this for every Kitty placement it encodes; a later
/// scroll uses the registration to crop a partially hidden placement.
pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    let mut registry = kitty_metadata_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.generation += 1;
    let transmission_generation = registry.generation;
    registry.max_rows = registry.max_rows.max(metadata.rows);
    registry.entries.retain(|(id, _)| *id != metadata.image_id);
    registry.entries.push((
        metadata.image_id,
        RegisteredKittyImageMetadata {
            metadata,
            transmission_generation,
        },
    ));
    if registry.entries.len() > MAX_KITTY_IMAGE_METADATA {
        registry.entries.remove(0);
    }
}

/// Row height of the tallest Kitty image registered so far (upstream
/// `getMaxKittyImageRows`). Bounds backward searches for image lines above a
/// viewport: no image line further up than this can reach into it.
pub fn get_max_kitty_image_rows() -> usize {
    kitty_metadata_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .max_rows
}

/// Byte bounds of the first Kitty command in `line`: the start of `\x1b_G`
/// and the index of its terminating `;` (upstream `/\x1b_G([^;]*);/`; the
/// leftmost `\x1b_G` always matches when any `;` follows it).
fn kitty_command_bounds(line: &str) -> Option<(usize, usize)> {
    let start = line.find(KITTY_PREFIX)?;
    let semicolon = line[start + KITTY_PREFIX.len()..].find(';')? + start + KITTY_PREFIX.len();
    Some((start, semicolon))
}

/// The `i=<id>` control of a Kitty control string (upstream
/// `/(?:^|,)i=(\d+)(?:,|$)/`).
fn kitty_image_id_from_controls(controls: &str) -> Option<u32> {
    let bytes = controls.as_bytes();
    let mut search = 0usize;
    while let Some(offset) = controls[search..].find("i=") {
        let index = search + offset;
        let digits_start = index + 2;
        let mut end = digits_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let preceded_ok = index == 0 || bytes[index - 1] == b',';
        let followed_ok = end == bytes.len() || bytes[end] == b',';
        if preceded_ok && end > digits_start && followed_ok {
            return controls[digits_start..end].parse().ok();
        }
        search = digits_start;
    }
    None
}

/// An explicit `r=<n>` control with `n > 0` (upstream
/// `getExplicitKittyImageRows`); a found `r=0` still ends the search.
fn explicit_kitty_image_rows(controls: &str) -> Option<usize> {
    let bytes = controls.as_bytes();
    let mut search = 0usize;
    while let Some(offset) = controls[search..].find("r=") {
        let index = search + offset;
        let digits_start = index + 2;
        let mut end = digits_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let preceded_ok = index == 0 || bytes[index - 1] == b',';
        let followed_ok = end == bytes.len() || bytes[end] == b',';
        if preceded_ok && end > digits_start && followed_ok {
            let rows = controls[digits_start..end].parse::<usize>().ok()?;
            return (rows > 0).then_some(rows);
        }
        search = digits_start;
    }
    None
}

fn registered_kitty_metadata_from_controls(controls: &str) -> Option<RegisteredKittyImageMetadata> {
    let image_id = kitty_image_id_from_controls(controls)?;
    let registry = kitty_metadata_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry
        .entries
        .iter()
        .find(|(id, _)| *id == image_id)
        .map(|(_, entry)| *entry)
}

/// The registered metadata for the first Kitty command in `line` (upstream
/// `getKittyImageMetadata`): `None` when the line carries no `\x1b_G … ;` or
/// its `i=` id was never registered.
pub fn get_kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    let (start, semicolon) = kitty_command_bounds(line)?;
    let controls = &line[start + KITTY_PREFIX.len()..semicolon];
    registered_kitty_metadata_from_controls(controls).map(|entry| entry.metadata)
}

/// Rows covered by an image placement without scanning its payload (upstream
/// `getKittyImagePlacementRows`): the explicit `r=` when present, otherwise
/// the registered row count.
pub fn get_kitty_image_placement_rows(line: &str) -> Option<usize> {
    let (start, semicolon) = kitty_command_bounds(line)?;
    let controls = &line[start + KITTY_PREFIX.len()..semicolon];
    if let Some(rows) = explicit_kitty_image_rows(controls) {
        return Some(rows);
    }
    registered_kitty_metadata_from_controls(controls).map(|entry| entry.metadata.rows)
}

/// Control keys a placement-only command keeps (upstream
/// `KITTY_PLACEMENT_CONTROL_KEYS`).
const KITTY_PLACEMENT_CONTROL_KEYS: &[&str] = &[
    "i", "p", "x", "y", "w", "h", "X", "Y", "c", "r", "C", "U", "z", "P", "Q", "H", "V",
];

/// One placement-only command derived from a rendered Kitty image line
/// (upstream `KittyImagePlacement`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImagePlacement {
    pub image_id: u32,
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: u64,
    pub rows: usize,
    /// The upload-free re-placement command (`a=p`).
    pub sequence: String,
    /// `line` with the transmission replaced by [`Self::sequence`].
    pub replacement_line: String,
}

/// Whether a control string carries the `m=1` continuation flag (upstream
/// `/(?:^|,)m=1(?:,|$)/`).
fn has_m_1_control(controls: &str) -> bool {
    let bytes = controls.as_bytes();
    let mut search = 0usize;
    while let Some(offset) = controls[search..].find("m=1") {
        let index = search + offset;
        let end = index + 3;
        let preceded_ok = index == 0 || bytes[index - 1] == b',';
        let followed_ok = end == bytes.len() || bytes[end] == b',';
        if preceded_ok && followed_ok {
            return true;
        }
        search = index + 1;
    }
    false
}

/// Build a placement-only command for an image line emitted by
/// [`render_image`] (upstream `getKittyImagePlacement`): `None` when the line
/// has no Kitty command or the image id is not registered. Walks the `m=1`
/// chunk chain to the end of the transmission so `transmission_bytes` covers
/// every chunk.
pub fn get_kitty_image_placement(line: &str) -> Option<KittyImagePlacement> {
    let (match_start, semicolon) = kitty_command_bounds(line)?;
    let match_controls = &line[match_start + KITTY_PREFIX.len()..semicolon];
    let metadata = registered_kitty_metadata_from_controls(match_controls)?;

    let mut command_start = match_start;
    let mut command_controls = match_controls;
    let transmission_end;
    loop {
        let from = command_start + KITTY_PREFIX.len();
        let terminator = line.get(from..)?.find("\x1b\\")? + from;
        let end = terminator + 2;
        if !has_m_1_control(command_controls) {
            transmission_end = end;
            break;
        }
        command_start = end;
        let after = line.get(command_start..)?.strip_prefix(KITTY_PREFIX)?;
        let controls_end = after.find(';')? + command_start + KITTY_PREFIX.len();
        command_controls = &line[command_start + KITTY_PREFIX.len()..controls_end];
    }

    let controls: Vec<&str> = match_controls
        .split(',')
        .filter(|control| {
            control
                .split('=')
                .next()
                .is_some_and(|key| KITTY_PLACEMENT_CONTROL_KEYS.contains(&key))
        })
        .collect();
    let sequence = format!("\x1b_Ga=p,q=2,{}\x1b\\", controls.join(","));
    Some(KittyImagePlacement {
        image_id: metadata.metadata.image_id,
        transmission_generation: metadata.transmission_generation,
        transmission_bytes: transmission_end - match_start,
        estimated_decoded_bytes: u64::from(metadata.metadata.width_px)
            * u64::from(metadata.metadata.height_px)
            * 4,
        rows: explicit_kitty_image_rows(match_controls).unwrap_or(metadata.metadata.rows),
        replacement_line: format!(
            "{}{}{}",
            &line[..match_start],
            sequence,
            &line[transmission_end..]
        ),
        sequence,
    })
}

/// Crop a Kitty image line to the rows still visible (upstream
/// `cropKittyImageLine`), so a placement that is partially scrolled away or
/// clipped by its box re-paints only the visible source rectangle: the
/// existing `y`/`h`/`r` controls are dropped and `y=<sourceY>`,
/// `h=<sourceHeight>`, `r=<croppedRows>` are appended in that order.
///
/// Returns `line` byte-identical in every no-op case: no registered metadata,
/// no `\x1b_G…;` match, `hidden_rows < 0`, `hidden_rows >= rows`,
/// `visible_rows <= 0`, or the whole image visible (`hidden_rows == 0` and
/// `cropped_rows == rows`).
pub fn crop_kitty_image_line(line: &str, hidden_rows: i64, visible_rows: i64) -> String {
    let metadata = match get_kitty_image_metadata(line) {
        Some(metadata) => metadata,
        None => return line.to_string(),
    };
    let Some((match_start, semicolon)) = kitty_command_bounds(line) else {
        return line.to_string();
    };
    let rows = metadata.rows as i64;
    if hidden_rows < 0 || hidden_rows >= rows || visible_rows <= 0 {
        return line.to_string();
    }
    let cropped_rows = visible_rows.min(rows - hidden_rows);
    if hidden_rows == 0 && cropped_rows == rows {
        return line.to_string();
    }
    // Integer arithmetic on i128 so the intermediate products cannot
    // overflow; all operands are non-negative past the guards, so `/` floors
    // and the `+ rows - 1` form is the ceiling division (upstream
    // `Math.floor` / `Math.ceil` on the same products).
    let height_px = i128::from(metadata.height_px);
    let hidden = i128::from(hidden_rows);
    let cropped = i128::from(cropped_rows);
    let rows_wide = i128::from(rows);
    let source_y = height_px * hidden / rows_wide;
    let source_end = (height_px * (hidden + cropped) + rows_wide - 1) / rows_wide;
    let source_height = (height_px.min(source_end) - source_y).max(1);

    let mut controls: Vec<String> = line[match_start + KITTY_PREFIX.len()..semicolon]
        .split(',')
        .filter(|control| {
            !(control.starts_with("y=") || control.starts_with("h=") || control.starts_with("r="))
        })
        .map(str::to_string)
        .collect();
    controls.push(format!("y={source_y}"));
    controls.push(format!("h={source_height}"));
    controls.push(format!("r={cropped_rows}"));

    let mut out = String::with_capacity(line.len() + 24);
    out.push_str(&line[..match_start]);
    out.push_str(KITTY_PREFIX);
    out.push_str(&controls.join(","));
    out.push(';');
    out.push_str(&line[semicolon + 1..]);
    out
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

/// One cached transcode: the payload key, its PNG base64 result, and the
/// result's byte length (`0` for a negative-cache `None`). The payload is
/// shared as `Arc<str>` so a cache hit clones a pointer, not up to ~85 MiB of
/// base64 text.
struct PngCacheEntry {
    key: u64,
    png: Option<Arc<str>>,
    size: usize,
}

/// Entry bound of the transcode cache (upstream `pngCache` bound).
const MAX_PNG_CACHE_ENTRIES: usize = 32;

/// Total-byte bound of the transcode cache. A single decode may be 64 MiB
/// (≈85 MiB once base64-encoded); 32 such entries would pin gigabytes in a
/// long TUI session, so the cache is bounded by total size as well.
const MAX_PNG_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Process-wide cache of transcoded PNG base64 payloads, keyed by a hash of
/// the source payload + MIME. A failed conversion is cached as `None`:
/// sending the same broken bytes to the decoder on every frame would repeat
/// the failure at frame rate. `entries[0]` is the newest; eviction drops from
/// the tail until both the entry and total-byte bounds hold.
#[derive(Default)]
struct PngCache {
    entries: Vec<PngCacheEntry>,
    bytes: usize,
}

impl PngCache {
    fn lookup(&self, key: u64) -> Option<Option<Arc<str>>> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.png.clone())
    }

    fn insert(&mut self, key: u64, png: Option<Arc<str>>, max_entries: usize, max_bytes: usize) {
        self.remove(key);
        let size = png.as_ref().map_or(0, |png| png.len());
        self.entries.insert(0, PngCacheEntry { key, png, size });
        self.bytes += size;
        while self.entries.len() > max_entries || self.bytes > max_bytes {
            let Some(evicted) = self.entries.pop() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(evicted.size);
        }
    }

    fn remove(&mut self, key: u64) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let entry = self.entries.remove(index);
            self.bytes = self.bytes.saturating_sub(entry.size);
        }
    }
}

fn png_cache() -> &'static std::sync::Mutex<PngCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<PngCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(PngCache::default()))
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
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()?;
    if decoded.is_empty() || decoded.len() > MAX_BYTES {
        return None;
    }
    let image = super::image_preview::decode_image_bounded(&decoded).ok()?;
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
fn cached_png_base64(base64_data: &str, mime_type: &str) -> Option<Arc<str>> {
    let key = png_cache_key(base64_data, mime_type);
    {
        let cache = png_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.lookup(key) {
            return cached;
        }
    }
    let png = transcode_to_png_base64(base64_data, mime_type).map(Arc::<str>::from);
    let mut cache = png_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.insert(key, png.clone(), MAX_PNG_CACHE_ENTRIES, MAX_PNG_CACHE_BYTES);
    png
}

/// Drop every cached transcode (test seam: the cache is process-global).
#[doc(hidden)]
pub fn png_cache_clear_for_test() {
    let mut cache = png_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *cache = PngCache::default();
}

/// Insert one synthetic transcode under explicit bounds and return the
/// resulting `(entries, bytes)` (test seam). Production inserts use
/// [`MAX_PNG_CACHE_ENTRIES`] / [`MAX_PNG_CACHE_BYTES`].
#[doc(hidden)]
pub fn png_cache_insert_for_test(
    key: u64,
    png: Option<String>,
    max_entries: usize,
    max_bytes: usize,
) -> (usize, usize) {
    let mut cache = png_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.insert(key, png.map(Arc::<str>::from), max_entries, max_bytes);
    (cache.entries.len(), cache.bytes)
}

/// Current `(entries, bytes)` of the process-wide cache (test seam).
#[doc(hidden)]
pub fn png_cache_stats_for_test() -> (usize, usize) {
    let cache = png_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (cache.entries.len(), cache.bytes)
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
            // Upstream `renderImage` registers the placement's metadata
            // (`terminal-image.ts:704-713`) so a later scroll can crop it;
            // upstream registers `options.imageId`, pir allocates the id on
            // this branch, so the registration is unconditional here.
            register_kitty_image_metadata(KittyImageMetadata {
                image_id,
                columns,
                rows,
                width_px,
                height_px,
            });
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
    // `mime_type` can come from an MCP/tool result; sanitize it before it is
    // spliced into a line that bypasses the Markdown renderer.
    let mime_type = crate::text::sanitize_terminal_text(mime_type);
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
