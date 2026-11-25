//! Public-surface parity with upstream `packages/coding-agent/src/index.ts`
//! (and the `packages/ai/src/index.ts` helpers that index re-exports).
//!
//! Everything here is a re-export or a small alias/naming adapter over an
//! implementation that already exists in the crate. There is no second
//! implementation of the agent loop, providers, tools, session storage or the
//! TUI: an item that upstream exposes from `index.ts` but that has no honest
//! Rust equivalent is documented as not exported in the module sections below,
//! never stubbed.
//!
//! The module is compiled only with the `sdk` feature (like the rest of
//! `pir_core::sdk`). Each section additionally gates on the cargo feature that owns
//! the implementation:
//!
//! | Section | Additional feature |
//! |---|---|
//! | model resolution / session stats | none (core) |
//! | file mutation queue | none (core) |
//! | image utilities | `print` or `tool-read`; resize needs the `image` crate |
//! | print mode | `print` |
//! | RPC host | `rpc` |
//! | RPC client | `client` |
//! | interactive mode / theme / keybindings | `tui` |
//!
//! ## Not exported (documented, not faked)
//!
//! - **`main` / `MainOptions`** — the CLI entry point lives in the `pir`
//!   binary (`src/main.rs`), not in the library. Embedders choose a run mode
//!   (`run_print_mode`, `run_interactive_mode`, `run_rpc_mode`) or build a
//!   session with [`crate::sdk::AgentBuilder`].
//! - **`Args` / `parseArgs`** — the clap argument schema is the binary's
//!   `cli` feature; a library embedding builds `PrintModeOptions` /
//!   `InteractiveModeOptions` directly instead of parsing argv.
//! - **`readStoredCredential`** — credential storage is
//!   [`crate::ai::auth::FileAuthStorage`] (`read_stored`, `store`,
//!   `list_stored`); the SDK does not wrap it a second time.
//! - **Compaction helpers** — [`crate::compaction`] is a different,
//!   entry-oriented engine (`prepare_compaction`, `compact_context`, ...);
//!   the upstream function names are not aliased onto it because the call
//!   contracts differ.
//! - **Skills / prompt-template helpers** — [`crate::skills`] and
//!   [`crate::coding_agent::prompt_templates`] are already public; the SDK
//!   re-exports only the types its own loader returns.
//! - **Interactive TUI components** (`AssistantMessageComponent`,
//!   `CustomEditor`, selector components, diff renderers, ...) — upstream's
//!   component classes are Node/pi-tui classes. pir's TUI is a native
//!   crossterm event loop; its renderer internals are not an extension API.
//!   The supported theme/keybinding helpers are the functions below.
//! - **`getSelectListTheme` / `getSettingsListTheme`** — pir's selectors read
//!   the process theme through `with_theme`; there is no separate theme
//!   object to hand out, so these would be empty shells.
//! - **`convertToPng` / terminal PNG transcoding** — the terminal image
//!   protocol path lives in the TUI (`terminal_image`); the SDK exposes the
//!   model-facing resize pipeline (`resize_image`) instead.
//! - **`copyToClipboard`** — the clipboard is a TUI feature
//!   ([`crate::tui::clipboard`]); a headless embedder supplies its own.
//! - **`IF`/`Type`/TypeBox helpers from `packages/ai`** — Rust uses serde
//!   JSON schemas (`serde_json::Value`) and serde types; there is no runtime
//!   schema builder to re-export. The AI model/message/event types remain
//!   available at [`crate::ai`], which is a public module of this crate.

use std::path::Path;

// ---------------------------------------------------------------------------
// Model resolution and diagnostics (upstream `core/model-resolver.ts`,
// `core/model-runtime.ts`)
// ---------------------------------------------------------------------------

pub use crate::agent_session::{
    ContextUsage, ModelCycle as ModelCycleResult, SessionStats, SessionTokens,
};
pub use crate::ai::providers::resolution::{
    default_model_per_provider, find_exact_model_reference_match, find_initial_model,
    parse_thinking_level, resolve_cli_model, resolve_cli_model_with_auth, resolve_model_reference,
    resolve_model_scope, InitialModelOptions, InitialModelResult, ModelReferenceRequest,
    ModelReferenceResult, ModelReferenceTarget, ScopedModel, ThinkingSuffixMode,
};
pub use crate::config::settings::{
    collect_settings_diagnostics, SettingsDiagnostic, SettingsDiagnosticType,
};

/// Upstream `AgentSessionRuntimeDiagnostic`: one startup info/warning/error
/// record.
///
/// Alias, not a second type: [`SettingsDiagnostic`] is the same severity +
/// message pair (`type_`/`message`, with `stderr_prefix`/`stderr_line`
/// helpers), and the settings manager collects it. Upstream's field is named
/// `type`; Rust cannot, so it is `type_` on the alias.
pub type AgentSessionRuntimeDiagnostic = SettingsDiagnostic;

/// Upstream `ModelRuntime`: the model/auth environment the resolver reads.
///
/// pir already has exactly one owned lookup face over
/// [`crate::ai::provider::ProviderRegistry`] — [`super::registry::ModelRegistry`]
/// (`from_environment` is the upstream `ModelRuntime.create` path,
/// `has_configured_auth` the auth filter). OAuth login/refresh and dynamic
/// provider registration stay in [`crate::ai::auth`] /
/// [`crate::ai::providers`]; this alias keeps the upstream type name without
/// duplicating that machinery.
pub type ModelRuntime = super::registry::ModelRegistry;

/// Upstream `ModelScopeDiagnostic`.
///
/// The Rust resolver ([`resolve_model_scope`]) returns the rendered warning
/// text (the same text upstream puts into the diagnostic's `message` field),
/// so the alias is the native `String` rather than a second structured type
/// that no resolver would construct.
pub type ModelScopeDiagnostic = String;

/// Upstream `ResolveModelScopeResult`: the resolver's native pair
/// (`scoped_models`, `diagnostics`).
pub type ResolveModelScopeResult = (Vec<ScopedModel>, Vec<ModelScopeDiagnostic>);

/// Upstream `ResolveCliModelResult`: `(model, thinking_level, warning)`.
pub type ResolveCliModelResult = (
    Option<crate::ai::types::Model>,
    Option<crate::ai::types::ThinkingLevel>,
    Option<String>,
);

// ---------------------------------------------------------------------------
// Tool output helpers (upstream `core/tools/index.ts`)
// ---------------------------------------------------------------------------

/// Upstream's tool-output truncation helpers (`DEFAULT_MAX_BYTES`,
/// `DEFAULT_MAX_LINES`, `formatSize`, `truncateHead`, `truncateTail`,
/// `TruncationResult`).
///
/// The Rust functions take the two limits explicitly
/// (`truncate_head(content, max_lines, max_bytes)`) instead of upstream's
/// optional options object with the same defaults
/// ([`DEFAULT_MAX_LINES`]/[`DEFAULT_MAX_BYTES`]).
pub use crate::coding_agent::truncate::{
    format_size, truncate_head, truncate_tail, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES,
};

// ---------------------------------------------------------------------------
// `withFileMutationQueue` (upstream `core/tools/index.ts`)
// ---------------------------------------------------------------------------

pub use crate::agent::tool_dependencies::{MutationQueues, ResourceKey};

/// Upstream `withFileMutationQueue`: run `operation` while holding the
/// mutation lock for one file path.
///
/// The path is canonicalized against `cwd` (the same
/// [`ResourceKey::canonical_async`] rule the tool scheduler uses, including
/// the logical fallback for files that do not exist yet), so `./a`, `a`,
/// `sub/../a` and case variants on Windows serialize on one lock while
/// distinct files proceed concurrently. The returned [`MutationQueues`] is
/// reusable across calls; the lock entry is evicted once idle.
///
/// Deviation: upstream keys an implicitly process-global queue; Rust makes
/// the queue table explicit so an embedder can scope it to one session (the
/// scheduler does exactly that with `MutationQueues::default()`).
pub async fn with_file_mutation_queue<T>(
    queues: &MutationQueues,
    path: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
    operation: impl std::future::Future<Output = T>,
) -> std::io::Result<T> {
    let key = ResourceKey::canonical_async(path.as_ref(), cwd.as_ref()).await?;
    Ok(queues.run_locked(&[key], operation).await)
}

// ---------------------------------------------------------------------------
// Image utilities (upstream `utils/mime.ts`, `utils/image-resize.ts`,
// `utils/image-convert.ts`)
// ---------------------------------------------------------------------------

/// Upstream `detectSupportedImageMimeTypeFromFile`: sniff the first bytes of
/// a file for an inline-image MIME type (`image/jpeg`, `image/png`,
/// `image/gif`, `image/webp`, `image/bmp`), returning `None` for non-images
/// and for the animated/unsupported variants upstream refuses.
///
/// This is the exact sniffer the `read` tool and the print host use
/// (`coding_agent::tools::image`), not a copy. Available whenever that module
/// compiles (`print` or `tool-read`); image decoding itself needs the `image`
/// crate (`image-tools`, or `print` + `tui` for the print attachment path).
#[cfg(any(feature = "print", feature = "tool-read"))]
pub fn detect_supported_image_mime_type_from_file(
    path: &Path,
) -> std::io::Result<Option<&'static str>> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path)?;
    let mut head = [0_u8; crate::coding_agent::tools::image::IMAGE_TYPE_SNIFF_BYTES];
    let read = file.read(&mut head)?;
    Ok(crate::coding_agent::tools::image::detect_supported_image_mime_type(&head[..read]))
}

/// Options for [`resize_image`] (upstream `ImageResizeOptions`).
///
/// Defaults match upstream: 2000x2000, 4.5MB of base64 payload (below
/// Anthropic's 5MB request limit), JPEG quality 80.
#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageResizeOptions {
    /// Maximum decoded width in pixels.
    pub max_width: u32,
    /// Maximum decoded height in pixels.
    pub max_height: u32,
    /// Maximum base64 payload size in bytes.
    pub max_bytes: usize,
    /// JPEG quality used by the JPEG candidates.
    pub jpeg_quality: u8,
}

#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
impl Default for ImageResizeOptions {
    fn default() -> Self {
        Self {
            max_width: 2_000,
            max_height: 2_000,
            max_bytes: 4_500_000,
            jpeg_quality: 80,
        }
    }
}

/// A processed inline image (upstream `ResizedImage`).
///
/// `data` is base64 (without a `data:` prefix), matching upstream.
/// `hints` carries the model-facing notes the resize pipeline appends: a
/// format-conversion note when the input was normalized (for example BMP to
/// PNG) and the dimension note (`original WxH, displayed at wxh`). Upstream's
/// separate `originalWidth`/`originalHeight`/`width`/`height`/`wasResized`
/// fields are not duplicated here; the same information is in `hints`, which
/// is what the read tool actually attaches to the message.
#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizedImage {
    /// Base64 payload (no data-URL prefix).
    pub data: String,
    /// Normalized MIME type of `data`.
    pub mime_type: String,
    /// Model-facing notes (conversion and/or dimension hints).
    pub hints: Vec<String>,
}

/// Why [`resize_image`] produced no inline image (upstream `resizeImage`
/// returning `null` for both cases).
#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageResizeError {
    /// The bytes could not be normalized to a supported inline format.
    Convert,
    /// No PNG/JPEG candidate fit `max_bytes`.
    Resize,
}

#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
impl std::fmt::Display for ImageResizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Convert => formatter.write_str(
                "[Image omitted: could not be converted to a supported inline image format.]",
            ),
            Self::Resize => formatter.write_str(
                "[Image omitted: could not be resized below the inline image size limit.]",
            ),
        }
    }
}

#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
impl std::error::Error for ImageResizeError {}

/// Upstream `resizeImage`: normalize to an inline format, resize to fit
/// `options`, and encode as base64.
///
/// This is the same `process_image` pipeline the `read` tool uses (EXIF
/// orientation, PNG first then the JPEG quality ladder, progressive shrinking
/// to the byte limit), exposed as a public function for custom tool
/// implementations. It is synchronous; call it from `spawn_blocking` on a
/// latency-sensitive async host.
#[cfg(all(
    feature = "image-resize",
    any(feature = "print", feature = "tool-read")
))]
pub fn resize_image(
    bytes: &[u8],
    mime_type: &str,
    options: ImageResizeOptions,
) -> Result<ResizedImage, ImageResizeError> {
    let processed = crate::coding_agent::tools::image::process_image(
        bytes,
        mime_type,
        options.max_width,
        options.max_height,
        options.max_bytes,
        options.jpeg_quality,
    )
    .map_err(|error| match error {
        crate::coding_agent::tools::image::ProcessImageError::Convert => ImageResizeError::Convert,
        crate::coding_agent::tools::image::ProcessImageError::Resize => ImageResizeError::Resize,
    })?;
    Ok(ResizedImage {
        data: processed.data_base64,
        mime_type: processed.mime_type,
        hints: processed.hints,
    })
}

// ---------------------------------------------------------------------------
// Print mode (upstream `runPrintMode`, `modes/print-mode.ts`)
// ---------------------------------------------------------------------------

/// Upstream `PrintModeOptions` / `runPrintMode` and their result types.
#[cfg(feature = "print")]
pub use crate::coding_agent::print::{
    run_print as run_print_mode, OutputMode, PrintOptions as PrintModeOptions, PrintOutcome,
};

// ---------------------------------------------------------------------------
// RPC host and client (upstream `runRpcMode`, `RpcClient`)
// ---------------------------------------------------------------------------

/// Upstream `runRpcMode`: serve one JSON-line RPC session on stdio.
///
/// Re-export of [`crate::coding_agent::rpc::run_rpc`]. It takes the extension
/// paths the host should load instead of upstream's options object.
#[cfg(feature = "rpc")]
pub use crate::coding_agent::rpc::run_rpc as run_rpc_mode;

/// Upstream `RpcClient` and its option/error/handle types
/// (`modes/rpc/rpc-client.ts`).
#[cfg(feature = "client")]
pub use crate::client::rpc::{
    RpcClient, RpcClientError, RpcClientOptions, RpcEventSubscription, CLOSE_GRACE,
    EVENT_CHANNEL_CAPACITY, IDLE_TIMEOUT, REQUEST_TIMEOUT, STDERR_CAPTURE_LIMIT,
};

/// Upstream `RpcCommand`: one command object sent to the host.
///
/// The Rust client speaks the same JSON-line protocol, so the payload is a
/// [`serde_json::Value`] rather than a generated enum: command coverage is
/// the [`RpcClient`] methods (`prompt`, `get_state`, `set_model`, ...), which
/// build these objects.
#[cfg(feature = "client")]
pub type RpcCommand = serde_json::Value;

/// Upstream `RpcResponse`: one `{type:"response", id, command, success, ...}`
/// frame (returned verbatim by [`RpcClient::send`]).
#[cfg(feature = "client")]
pub type RpcResponse = serde_json::Value;

/// Upstream `RpcSessionState`: the `get_state` payload.
#[cfg(feature = "client")]
pub type RpcSessionState = serde_json::Value;

/// Upstream `RpcExtensionUIRequest`: an extension UI request event.
#[cfg(feature = "client")]
pub type RpcExtensionUIRequest = serde_json::Value;

/// Upstream `RpcExtensionUIResponse`: the matching `extension_ui_response`.
#[cfg(feature = "client")]
pub type RpcExtensionUIResponse = serde_json::Value;

/// Upstream `RpcEventListener`: a synchronous event callback.
///
/// [`RpcClient::on_event`] accepts the same closure shape directly (and its
/// returned [`RpcEventSubscription`] unregisters on drop).
#[cfg(feature = "client")]
pub type RpcEventListener = dyn Fn(&serde_json::Value) + Send + Sync;
