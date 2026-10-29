//! Read tool: reads text file contents with offset/limit, returns image
//! content blocks for jpg/png/gif/webp/bmp files (or the explicit
//! compiled-without-`image-tools` note when this build has no `image` crate),
//! and appends upstream-style truncation footers.
//!
//! Text reads are streamed: only the leading sniff window and the selected
//! line range are retained, so a deep offset in a large file costs bounded
//! memory instead of a full-file buffer + line vector + join copy. The whole
//! file is still traversed (chunked, abortable) because the continuation
//! footers report the total line count.

use std::path::{Path, PathBuf};

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use crate::ai::types::AbortSignal;
use crate::coding_agent::truncate::{
    format_size, TruncatedBy, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};
use tokio::io::AsyncReadExt;

// The MIME sniffer is crate-free and always available; the decode/resize half
// exists only when the `image` crate is compiled in for the read TOOL
// (`image-tools`). `tui` also carries `dep:image`, but for its own preview /
// settings code — it must not enable read's attachment path (docs/FEATURES.md).
use super::confine::ConfinedTarget;
use super::image::{detect_supported_image_mime_type, IMAGE_TYPE_SNIFF_BYTES};
#[cfg(feature = "image-tools")]
use super::image::{
    process_image, DEFAULT_JPEG_QUALITY, DEFAULT_MAX_BYTES as IMAGE_MAX_BYTES, DEFAULT_MAX_HEIGHT,
    DEFAULT_MAX_WIDTH,
};
use super::util::{
    max_source_bytes, normalize_tool_path_remote, source_limit_error, workspace_path_parts,
    workspace_path_parts_normalized, WorkspacePathParts,
};

/// Leading window read before deciding image/binary/text. Covers the 8 KiB
/// NUL sniff and the 4100-byte image sniff.
const HEAD_SNIFF_BYTES: usize = 8192;

/// Chunk size for streaming text reads and whole-file image reads; every read
/// is raced with the abort signal.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// One line is captured up to this many bytes; longer lines are measured but
/// never retained (they cannot fit the 50 KB output budget anyway).
const LINE_CAPTURE_BYTES: usize = DEFAULT_MAX_BYTES + 4096;

/// A leading UTF-8 BOM is stripped for display and line counting (the edit
/// tool's `splitBom`, edit.rs:32-41).
const BOM: char = '\u{FEFF}';

/// The inline-image support the read tool resolves before reading: model
/// vision support and the model's resize limits when the `image-tools`
/// feature is on, unit when it is not — the image branch then reports the
/// compiled-out degradation instead of an image block. `tui` deliberately
/// does not imply this: it carries `dep:image` for its own preview/settings
/// code only.
#[cfg(feature = "image-tools")]
type ImageSupport = (bool, (u32, u32, usize, u8));
#[cfg(not(feature = "image-tools"))]
type ImageSupport = ();

// ---------------------------------------------------------------------------
// Read tool
// ---------------------------------------------------------------------------

/// The tool description: the upstream text when `image-tools` can attach
/// images, and a variant that says they cannot otherwise (the tool then never
/// returns image content, so it must not claim to).
#[cfg(feature = "image-tools")]
fn read_description() -> &'static str {
    "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
}

#[cfg(not(feature = "image-tools"))]
fn read_description() -> &'static str {
    "Read the contents of a file. Supports text files. Images (jpg, png, gif, webp, bmp) are recognized but not attached: this build was compiled without the `image-tools` feature, so reading an image reports its type and size only. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
}

/// Upstream `readOutputSchema`: the structured output for programmatic
/// callers — the text for text files, or an image object with its note. A
/// build without `image-tools` can only ever produce the text shape.
#[cfg(feature = "image-tools")]
fn read_output_schema() -> serde_json::Value {
    serde_json::json!({
        "anyOf": [
            { "type": "string" },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "image" },
                    "data": { "type": "string" },
                    "mimeType": { "type": "string" },
                    "note": { "type": "string" }
                },
                "required": ["type", "data", "mimeType", "note"]
            }
        ]
    })
}

#[cfg(not(feature = "image-tools"))]
fn read_output_schema() -> serde_json::Value {
    serde_json::json!({ "type": "string" })
}

/// Heuristic binary detection: a NUL byte anywhere in the first 8KB marks the
/// file as binary.
fn is_binary_content(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    head.contains(&0)
}

/// A JSON number argument as a finite, non-negative `f64`.
///
/// The schemas declare `Type.Number` (upstream), so JSON floats such as `5.0`
/// are legitimate inputs; `as_i64` silently rejected them and fell back to the
/// default (line 1 / unlimited). Non-finite values are rejected; negative
/// values clamp to zero; callers truncate toward zero like grep/find/ls.
fn number_arg(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite())
        .map(|value| value.max(0.0))
}

/// Upstream `getNonVisionImageNote`: the note appended when the current model
/// cannot receive images (the transform layer then drops the block itself).
/// Only the crate-backed image branch emits image blocks, so this is gated
/// with it.
#[cfg(feature = "image-tools")]
fn non_vision_image_note(supports_images: bool) -> Option<&'static str> {
    if supports_images {
        None
    } else {
        Some(
            "[Current model does not support images. The image will be omitted from this request.]",
        )
    }
}

/// Pluggable filesystem operations for the read tool (upstream
/// `ReadOperations`, read.ts:48-70). Override these to delegate reads to a
/// remote system; path normalization and workspace confinement still run in
/// the tool before the backend is called.
pub trait ReadOperations: Send + Sync {
    /// Read the file contents.
    fn read_file<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<Vec<u8>>>;
    /// Check the file is readable (an error means it is not).
    fn access<'a>(&'a self, absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<()>>;
    /// Detect an image MIME type; `None` lets the built-in sniffer decide from
    /// the bytes.
    fn detect_image_mime_type<'a>(
        &'a self,
        _absolute_path: &'a Path,
    ) -> BoxFuture<'a, Option<&'static str>> {
        Box::pin(async { None })
    }
}

pub struct ReadTool {
    operations: Option<std::sync::Arc<dyn ReadOperations>>,
}

impl ReadTool {
    /// The built-in tool (local filesystem, or the `env_client` remote arm
    /// when configured).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit read backend (upstream
    /// `createReadTool(cwd, { operations })`). An injected backend wins over
    /// the process-wide `env_client` selection.
    pub fn with_operations(operations: std::sync::Arc<dyn ReadOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Upstream `tryMacOSScreenshotPath`: macOS screenshot names use U+202F
/// (narrow no-break space) before AM/PM, while users type a regular space.
fn am_pm_variant(path: &str) -> Option<String> {
    use std::sync::OnceLock;
    static PATTERN: OnceLock<Option<regex::Regex>> = OnceLock::new();
    let pattern = PATTERN
        .get_or_init(|| regex::Regex::new(r"(?i) (am|pm)\.").ok())
        .as_ref()?;
    if !pattern.is_match(path) {
        return None;
    }
    Some(
        pattern
            .replace_all(path, |captures: &regex::Captures<'_>| {
                format!("\u{202F}{}.", &captures[1])
            })
            .into_owned(),
    )
}

/// The macOS filesystem compatibility variants of `path` (upstream
/// `resolveReadPath`, path-utils.ts:52-84): the AM/PM screenshot form, the NFD
/// (decomposed) form macOS stores, the U+2019 curly-quote form, and NFD+curly
/// combined. They are only consulted when the literal path does not exist.
fn read_path_variants(path: &Path) -> Vec<PathBuf> {
    use unicode_normalization::UnicodeNormalization;

    let original = path.to_string_lossy().into_owned();
    let nfd = original.nfd().collect::<String>();
    let curly = |value: &str| value.replace('\'', "\u{2019}");
    let mut variants: Vec<PathBuf> = Vec::new();
    let mut push = |candidate: String| {
        if candidate != original {
            let candidate = PathBuf::from(candidate);
            if !variants.contains(&candidate) {
                variants.push(candidate);
            }
        }
    };
    if let Some(am_pm) = am_pm_variant(&original) {
        push(am_pm);
    }
    push(nfd.clone());
    push(curly(&original));
    push(curly(&nfd));
    variants
}

/// Resolve a read target: the normalized path through workspace confinement,
/// then the upstream macOS variants when the literal file does not exist.
/// Every variant is re-submitted to the same confinement walk, so a variant
/// can never widen the workspace boundary.
fn resolve_read_path(cwd: &Path, requested: &str) -> Result<WorkspacePathParts, String> {
    let parts = workspace_path_parts(cwd, requested)?;
    if parts.candidate.exists() {
        return Ok(parts);
    }
    for variant in read_path_variants(&parts.candidate) {
        if !variant.exists() {
            continue;
        }
        let display = variant.to_string_lossy().into_owned();
        if let Ok(variant_parts) = workspace_path_parts_normalized(cwd, &variant, &display) {
            return Ok(variant_parts);
        }
    }
    Ok(parts)
}

impl AgentTool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        read_description()
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("Read file contents".to_owned())
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec!["Use read to examine files instead of cat or sed.".to_owned()]
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                // Upstream `readSchema` is `Type.Number` (`core/tools/read.ts:16-17`), not
                // `Type.Integer`: a float offset/limit is accepted and truncated
                // toward zero by the read body. Declaring `"integer"` here also kept
                // the `tool-repair` numeric-string rule (`type === "number"` only) from
                // ever coercing these two fields.
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        }))
    }
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(read_output_schema())
    }
    /// Upstream `read.ts:101`: the read tool prefers strict JSON-schema
    /// constrained sampling.
    fn constrained_sampling(&self) -> Option<crate::ai::types::ConstrainedSamplingConfig> {
        Some(crate::ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: crate::ai::types::StrictPreference::Prefer,
        })
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Upstream read.ts: offset is 1-indexed and the out-of-range error
        // reports the raw value; `limit` is honored before truncation. Both
        // arrive as JSON numbers (integer or float), truncated toward zero.
        let raw_offset = number_arg(args.get("offset"));
        let offset = raw_offset
            .map(|value| value.trunc() as usize)
            .map(|value| value.saturating_sub(1))
            .unwrap_or(0);
        let limit = number_arg(args.get("limit")).map(|value| value.trunc() as usize);
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();
        let aborted = signal.as_ref().is_some_and(AbortSignal::is_aborted);
        // The inline-image support resolved for this read: model vision
        // support (upstream `ctx.model`; without the context the image block is
        // emitted and the transform layer downgrades it for non-vision models)
        // plus the model's resize limits (upstream
        // `ctx.model.inputLimits.images.resize`; a model without the metadata
        // keeps the upstream defaults). Both need the `image` crate, so a build
        // without it passes the unit support and the image branch reports the
        // compiled-out degradation.
        #[cfg(feature = "image-tools")]
        let image_support: ImageSupport = (
            ctx.session_env
                .as_ref()
                .map(|env| env.model.input.iter().any(|input| input == "image"))
                .unwrap_or(true),
            image_resize_limits(ctx.session_env.as_deref().map(|env| &env.model)),
        );
        #[cfg(not(feature = "image-tools"))]
        let image_support: ImageSupport = ();
        // The env indirection layer (`env_client::selection`): `None` — the
        // default, no override and no `PI_ENV_SSH_HOST` — leaves the local path
        // below byte-identical to what it was before the layer existed.
        let remote = crate::env_client::configured_remote_env();
        // An explicitly injected backend wins over the process-wide env_client
        // selection (upstream `ReadToolOptions.operations`).
        let operations = self.operations.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error("Operation aborted");
            }
            if let Some(operations) = operations {
                return read_with_operations(
                    operations,
                    cwd,
                    path,
                    raw_offset,
                    offset,
                    limit,
                    image_support,
                    signal,
                )
                .await;
            }
            if let Some(environment) = remote {
                return read_remote(
                    environment,
                    &path,
                    raw_offset,
                    offset,
                    limit,
                    image_support,
                    signal,
                )
                .await;
            }
            // Name-based confinement first (model-facing errors), then the
            // read itself through a handle anchored at the verified parent
            // directory: a component swapped after the check cannot redirect
            // the open (the read arm had the same resolve-then-open window the
            // write/edit arms closed).
            let parts = match resolve_read_path(&cwd, &path) {
                Ok(parts) => parts,
                Err(error) => return ToolResult::error(error),
            };
            let target = match ConfinedTarget::open(&parts.root, &parts.relative, false) {
                Ok(target) => target,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return ToolResult::error(super::confine::confinement_error(&path, &error))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return ToolResult::error(format!(
                        "Path not found: {}",
                        parts.candidate.display()
                    ))
                }
                Err(error) => {
                    return ToolResult::error(format!("Cannot read '{}': {}", path, error))
                }
            };
            let file = match target.open_read() {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return ToolResult::error(format!(
                        "Path not found: {}",
                        parts.candidate.display()
                    ))
                }
                Err(error) => {
                    return ToolResult::error(format!("Cannot read '{}': {}", path, error))
                }
            };
            read_local(
                file,
                &path,
                raw_offset,
                offset,
                limit,
                image_support,
                signal,
            )
            .await
        })
    }
}

/// The pluggable-backend arm of the read tool: normalize + confine the path
/// exactly like the local arm, then obtain the bytes through the backend. The
/// image/binary/text shaping below is shared with the other arms, so a model
/// sees the same result shape from any backend.
// The per-call knobs are a fixed set threaded from the tool arm; a parameter
// struct would only repeat them at the single call site.
#[allow(clippy::too_many_arguments)]
async fn read_with_operations(
    operations: std::sync::Arc<dyn ReadOperations>,
    cwd: PathBuf,
    path: String,
    raw_offset: Option<f64>,
    offset: usize,
    limit: Option<usize>,
    image_support: ImageSupport,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let parts = match resolve_read_path(&cwd, &path) {
        Ok(parts) => parts,
        Err(error) => return ToolResult::error(error),
    };
    let target = parts.candidate;
    if let Err(error) = operations.access(&target).await {
        return custom_read_error(&path, &target, &error);
    }
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    let bytes = match operations.read_file(&target).await {
        Ok(bytes) => bytes,
        Err(error) => return custom_read_error(&path, &target, &error),
    };
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    let custom_mime = operations.detect_image_mime_type(&target).await;
    read_bytes(
        bytes,
        custom_mime,
        &path,
        raw_offset,
        offset,
        limit,
        image_support,
    )
    .await
}

/// The model-facing error for a backend access/read failure: the same
/// not-found wording the local arm uses; every other failure keeps the
/// backend's own OS error.
fn custom_read_error(path: &str, target: &Path, error: &std::io::Error) -> ToolResult {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            ToolResult::error(format!("Path not found: {}", target.display()))
        }
        _ => ToolResult::error(format!("Cannot read '{}': {}", path, error)),
    }
}

/// Shape an already-materialized byte buffer: the shared image/binary/text
/// decision and text rendering. Used by the pluggable backend arm (upstream
/// `readFile` returns a whole buffer there too).
async fn read_bytes(
    bytes: Vec<u8>,
    custom_mime: Option<&'static str>,
    path: &str,
    raw_offset: Option<f64>,
    offset: usize,
    limit: Option<usize>,
    image_support: ImageSupport,
) -> ToolResult {
    let sniff_head = &bytes[..bytes.len().min(IMAGE_TYPE_SNIFF_BYTES)];
    let mime_type = custom_mime.or_else(|| detect_supported_image_mime_type(sniff_head));
    if let Some(mime_type) = mime_type {
        if bytes.len() as u64 > max_source_bytes() {
            return ToolResult::error(format!(
                "Cannot read '{}': {}",
                path,
                source_limit_error(bytes.len() as u64)
            ));
        }
        return read_image(bytes, mime_type, image_support).await;
    }
    if is_binary_content(&bytes) {
        return ToolResult::error(format!("Cannot read '{}': binary file", path));
    }
    let end_line = match limit {
        Some(limit) => offset.saturating_add(limit),
        None => usize::MAX,
    };
    let mut scan = TextScan::new(offset, end_line);
    scan.push(&bytes);
    render_text(path, raw_offset, offset, limit, scan.finish())
}

/// Wait until the signal aborts; never resolves when no signal is installed.
async fn wait_for_abort(signal: Option<&AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// One aborted-able read: `Ok(None)` means the signal fired while waiting.
async fn read_chunk(
    file: &mut tokio::fs::File,
    buffer: &mut [u8],
    signal: Option<&AbortSignal>,
) -> std::io::Result<Option<usize>> {
    tokio::select! {
        result = file.read(buffer) => result.map(Some),
        _ = wait_for_abort(signal) => Ok(None),
    }
}

/// Read the rest of an opened file after `bytes`, chunked so cancellation is
/// observed between chunks.
async fn read_remaining(
    file: &mut tokio::fs::File,
    mut bytes: Vec<u8>,
    signal: Option<&AbortSignal>,
    max_bytes: u64,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        match read_chunk(file, &mut chunk, signal).await? {
            Some(0) => return Ok(Some(bytes)),
            Some(read) => {
                let next_len = (bytes.len() as u64).saturating_add(read as u64);
                if next_len > max_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        source_limit_error(next_len),
                    ));
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            None => return Ok(None),
        }
    }
}

/// The local arm: `file` is the already-open, handle-relative
/// (`O_NOFOLLOW`/reparse-checked) descriptor from
/// [`ConfinedTarget::open_read`]. Stat and sniff the head first (image/binary
/// decisions need at most 8 KiB), then either read the whole image or stream
/// the text line range.
async fn read_local(
    file: std::fs::File,
    path: &str,
    raw_offset: Option<f64>,
    offset: usize,
    limit: Option<usize>,
    image_support: ImageSupport,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let mut file = tokio::fs::File::from_std(file);
    let size = match tokio::select! {
        result = file.metadata() => result,
        _ = wait_for_abort(signal.as_ref()) => return ToolResult::error("Operation aborted"),
    } {
        Ok(metadata) => metadata.len(),
        Err(e) => return ToolResult::error(format!("Cannot read '{}': {}", path, e)),
    };
    // Stat first: the sniff buffer never exceeds the file size.
    let head_len = usize::try_from(size.min(HEAD_SNIFF_BYTES as u64)).unwrap_or(HEAD_SNIFF_BYTES);
    let mut head = vec![0u8; head_len];
    let mut filled = 0;
    while filled < head.len() {
        match read_chunk(&mut file, &mut head[filled..], signal.as_ref()).await {
            Ok(Some(0)) => break,
            Ok(Some(read)) => filled += read,
            Ok(None) => return ToolResult::error("Operation aborted"),
            Err(e) => return ToolResult::error(format!("Cannot read '{}': {}", path, e)),
        }
    }
    head.truncate(filled);
    // Sniff only the documented window; the head is at most 8 KiB.
    let sniff_head = &head[..head.len().min(IMAGE_TYPE_SNIFF_BYTES)];
    if let Some(mime_type) = detect_supported_image_mime_type(sniff_head) {
        let image_limit = max_source_bytes();
        if size > image_limit {
            return ToolResult::error(format!(
                "Cannot read '{}': {}",
                path,
                source_limit_error(size)
            ));
        }
        // Images are decoded as a whole; the remainder is read chunked and
        // abortable instead of a single uninterruptible `fs::read`.
        let bytes = match read_remaining(&mut file, head, signal.as_ref(), image_limit).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return ToolResult::error("Operation aborted"),
            Err(e) => return ToolResult::error(format!("Cannot read '{}': {}", path, e)),
        };
        if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return ToolResult::error("Operation aborted");
        }
        return read_image(bytes, mime_type, image_support).await;
    }
    if is_binary_content(&head) {
        return ToolResult::error(format!("Cannot read '{}': binary file", path));
    }
    let end_line = match limit {
        Some(limit) => offset.saturating_add(limit),
        None => usize::MAX,
    };
    let mut scan = TextScan::new(offset, end_line);
    scan.push(&head);
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        match read_chunk(&mut file, &mut chunk, signal.as_ref()).await {
            Ok(Some(0)) => break,
            Ok(Some(read)) => scan.push(&chunk[..read]),
            Ok(None) => return ToolResult::error("Operation aborted"),
            Err(e) => return ToolResult::error(format!("Cannot read '{}': {}", path, e)),
        }
    }
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return ToolResult::error("Operation aborted");
    }
    render_text(path, raw_offset, offset, limit, scan.finish())
}

/// The remote arm of the read tool: the same read through `RemoteExecutionEnv`
/// (the env indirection layer's `Some` branch). The confinement rule is the
/// remote analogue of `workspace_path` (same messages, evaluated on the
/// remote file system), and the content shaping below — image sniffing,
/// image processing, text slicing, truncation footers — is the *same code* as
/// the local arm, so both arms produce identical results for identical bytes.
/// The abort signal is transmitted in the per-call [`Context`], whose remote
/// `readFile` checks it before every chunk.
async fn read_remote(
    environment: std::sync::Arc<crate::env_client::RemoteExecutionEnv>,
    path: &str,
    raw_offset: Option<f64>,
    offset: usize,
    limit: Option<usize>,
    image_support: ImageSupport,
    signal: Option<AbortSignal>,
) -> ToolResult {
    let context = crate::env_client::Context::new(signal);
    // The remote path gets the same spelling normalization as the local arm's
    // first step (`@file`, Unicode spaces); `~` is left alone because the
    // remote environment's home directory is not this process's home.
    let requested_path = normalize_tool_path_remote(path);
    let file_path =
        match super::util::workspace_path_remote_checked(&environment, &requested_path).await {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error),
        };
    let mut reader = match environment
        .open_binary_reader(&file_path, true, &context)
        .await
    {
        Ok(reader) => reader,
        Err(error) => return remote_read_error(path, &file_path, &error),
    };
    let info = match reader.info(&context).await {
        Ok(info) => info,
        Err(error) => {
            let _ = reader.close().await;
            return remote_read_error(path, &file_path, &error);
        }
    };
    let size = info.size;
    let head_len = usize::try_from(size.min(HEAD_SNIFF_BYTES as u64)).unwrap_or(HEAD_SNIFF_BYTES);
    let head = match reader.read(0, head_len as u64, &context).await {
        Ok(head) => head,
        Err(error) => {
            let _ = reader.close().await;
            return remote_read_error(path, &file_path, &error);
        }
    };
    if context.aborted() {
        let _ = reader.close().await;
        return ToolResult::error("Operation aborted");
    }
    let sniff_head = &head[..head.len().min(IMAGE_TYPE_SNIFF_BYTES)];
    if let Some(mime_type) = detect_supported_image_mime_type(sniff_head) {
        let image_limit = max_source_bytes();
        if size > image_limit {
            let _ = reader.close().await;
            return ToolResult::error(format!(
                "Cannot read '{}': {}",
                path,
                source_limit_error(size)
            ));
        }
        let mut bytes = head;
        let mut position = bytes.len() as u64;
        while position < size {
            let want = (size - position).min(READ_CHUNK_BYTES as u64);
            let chunk = match reader.read(position, want, &context).await {
                Ok(chunk) => chunk,
                Err(error) => {
                    let _ = reader.close().await;
                    return remote_read_error(path, &file_path, &error);
                }
            };
            if chunk.is_empty() {
                break;
            }
            if (bytes.len() as u64).saturating_add(chunk.len() as u64) > image_limit {
                let _ = reader.close().await;
                return ToolResult::error(format!(
                    "Cannot read '{}': {}",
                    path,
                    source_limit_error(image_limit)
                ));
            }
            position = position.saturating_add(chunk.len() as u64);
            bytes.extend_from_slice(&chunk);
        }
        let _ = reader.close().await;
        if context.aborted() {
            return ToolResult::error("Operation aborted");
        }
        return read_image(bytes, mime_type, image_support).await;
    }
    if is_binary_content(&head) {
        let _ = reader.close().await;
        return ToolResult::error(format!("Cannot read '{}': binary file", path));
    }
    let end_line = match limit {
        Some(limit) => offset.saturating_add(limit),
        None => usize::MAX,
    };
    let mut scan = TextScan::new(offset, end_line);
    scan.push(&head);
    let mut position = head.len() as u64;
    while position < size {
        let want = (size - position).min(READ_CHUNK_BYTES as u64);
        let chunk = match reader.read(position, want, &context).await {
            Ok(chunk) => chunk,
            Err(error) => {
                let _ = reader.close().await;
                return remote_read_error(path, &file_path, &error);
            }
        };
        if chunk.is_empty() {
            break;
        }
        position = position.saturating_add(chunk.len() as u64);
        scan.push(&chunk);
    }
    let _ = reader.close().await;
    if context.aborted() {
        return ToolResult::error("Operation aborted");
    }
    render_text(path, raw_offset, offset, limit, scan.finish())
}

fn remote_read_error(
    path: &str,
    file_path: &str,
    error: &crate::env_client::FileError,
) -> ToolResult {
    use crate::env_client::FileErrorCode;
    match error.code {
        FileErrorCode::NotFound => ToolResult::error(format!("Path not found: {file_path}")),
        FileErrorCode::Aborted => ToolResult::error("Operation aborted"),
        _ => ToolResult::error(format!("Cannot read '{path}': {}", error.message)),
    }
}

/// Resolve the inline-image resize profile for `model` (upstream
/// `ctx.model.inputLimits?.images?.resize`). The value can come from
/// `Model.compat` (where models.json carries it) or from the top-level
/// `Model.inputLimits` used by the upstream wire shape. Fields the model does
/// not define fall back to the upstream defaults. Only the crate-backed image
/// branch consumes this, so it is gated with it.
#[cfg(feature = "image-tools")]
fn image_resize_limits(model: Option<&crate::ai::types::Model>) -> (u32, u32, usize, u8) {
    let input_limits = model.and_then(|model| {
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.input_limits.as_ref())
            .or(model.input_limits.as_ref())
    });
    let resize = input_limits
        .and_then(|limits| limits.get("images"))
        .and_then(|images| images.get("resize"));
    let dimension = |key: &str, default: u32| {
        resize
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    };
    let max_width = dimension("maxWidth", DEFAULT_MAX_WIDTH);
    let max_height = dimension("maxHeight", DEFAULT_MAX_HEIGHT);
    let max_bytes = resize
        .and_then(|value| value.get("maxBytes"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(IMAGE_MAX_BYTES);
    let jpeg_quality = resize
        .and_then(|value| value.get("jpegQuality"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| (1..=100).contains(value))
        .unwrap_or(DEFAULT_JPEG_QUALITY);
    (max_width, max_height, max_bytes, jpeg_quality)
}

/// Image branch when the `image` crate is compiled in (upstream
/// `processImage` + the image content block), with the caller-resolved vision
/// support and resize limits. Takes the bytes by value so the CPU-bound
/// decode owns them instead of cloning the whole file.
#[cfg(feature = "image-tools")]
async fn read_image(
    bytes: Vec<u8>,
    mime_type: &'static str,
    image_support: ImageSupport,
) -> ToolResult {
    let (supports_images, (max_width, max_height, max_bytes, jpeg_quality)) = image_support;
    // Decoding/encoding is CPU-bound: keep it off the async reactor thread.
    let processed = tokio::task::spawn_blocking(move || {
        process_image(
            &bytes,
            mime_type,
            max_width,
            max_height,
            max_bytes,
            jpeg_quality,
        )
    })
    .await;
    let processed = match processed {
        Ok(processed) => processed,
        Err(error) => return ToolResult::error(format!("Read image file [{mime_type}]: {error}")),
    };
    // Upstream: the text note is `Read image file [<processed mime>]` plus the
    // resize hints, or the failure message for a rejected image.
    let mut note = match &processed {
        Ok(processed) => format!("Read image file [{}]", processed.mime_type),
        Err(error) => format!("Read image file [{mime_type}]\n{}", error.message()),
    };
    let image_content = match &processed {
        Ok(processed) => {
            for hint in &processed.hints {
                note.push('\n');
                note.push_str(hint);
            }
            Some(crate::ai::types::ImageContent {
                data: processed.data_base64.clone(),
                mime_type: processed.mime_type.clone(),
            })
        }
        Err(_) => None,
    };
    if let Some(vision_note) = non_vision_image_note(supports_images) {
        note.push('\n');
        note.push_str(vision_note);
    }
    // Structured content mirrors upstream `toReadOutput`: the image object
    // with its note, or the plain text when no image was produced.
    let structured_content = match &image_content {
        Some(image) => serde_json::json!({
            "type": "image",
            "data": image.data,
            "mimeType": image.mime_type,
            "note": note,
        }),
        None => serde_json::Value::String(note.clone()),
    };
    let mut result = ToolResult::ok(note);
    if let Some(image) = image_content {
        result.images.push(image);
    }
    result.structured_content = Some(structured_content);
    result
}

/// Image branch when the `image` crate is compiled out (no `image-tools`, no
/// `tui`): the crate-free sniff still recognizes the file, so the result is
/// the same `Read image file [<mime>]` metadata line plus the explicit
/// degradation note with the file size — never a silent skip and never an
/// image block the tool cannot produce. `structured_content` is the note
/// string, the same shape the feature build uses when image processing fails.
#[cfg(not(feature = "image-tools"))]
async fn read_image(
    bytes: Vec<u8>,
    mime_type: &'static str,
    _image_support: ImageSupport,
) -> ToolResult {
    let note = format!(
        "Read image file [{mime_type}]\n[Image omitted: this build was compiled without the `image-tools` feature, so the image content was not attached. File size: {}. Rebuild with the `image-tools` feature to read images as attachments.]",
        format_size(bytes.len())
    );
    let mut result = ToolResult::ok(note.clone());
    result.structured_content = Some(serde_json::Value::String(note));
    result
}

/// Decoded byte length of one line (lossy UTF-8, BOM stripped from line 0).
fn decoded_line_len(bytes: &[u8], line_index: usize) -> usize {
    let decoded = String::from_utf8_lossy(bytes);
    if line_index == 0 {
        decoded.strip_prefix(BOM).unwrap_or(&decoded).len()
    } else {
        decoded.len()
    }
}

/// Owned decoded text of a retained line (lossy UTF-8, BOM stripped from
/// line 0).
fn decoded_line(bytes: &[u8], line_index: usize) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    if line_index == 0 {
        decoded.strip_prefix(BOM).unwrap_or(&decoded).to_owned()
    } else {
        decoded.into_owned()
    }
}

/// Streaming line scanner for text reads.
///
/// It retains only selected lines that `truncate_head` could return (at most
/// [`DEFAULT_MAX_LINES`] lines / [`DEFAULT_MAX_BYTES`] bytes); everything else
/// is measured while scanning. Memory is therefore bounded by the output
/// budget plus one captured line, not by the file size.
struct TextScan {
    start_line: usize,
    end_line: usize,
    kept: Vec<String>,
    kept_bytes: usize,
    /// Number of real lines in `kept` (the synthetic trailing empty piece is
    /// not a line, even though it is retained so `join("\n")` preserves the
    /// file's final newline).
    kept_lines: usize,
    selected_lines: usize,
    selected_bytes: usize,
    selected_last_empty: bool,
    first_line_len: usize,
    line_index: usize,
    current: Vec<u8>,
    current_len: usize,
    current_capped: bool,
    /// The first line that did not fit the byte budget stops retention for
    /// good (upstream `truncate_head` breaks out of its loop); later shorter
    /// lines must not be appended after it.
    byte_limit_hit: bool,
}

impl TextScan {
    fn new(start_line: usize, end_line: usize) -> Self {
        Self {
            start_line,
            end_line,
            kept: Vec::new(),
            kept_bytes: 0,
            kept_lines: 0,
            selected_lines: 0,
            selected_bytes: 0,
            selected_last_empty: false,
            first_line_len: 0,
            line_index: 0,
            current: Vec::new(),
            current_len: 0,
            current_capped: false,
            byte_limit_hit: false,
        }
    }

    /// Feed a byte slice; line splits happen on `\n` (a byte that can never
    /// appear inside a multi-byte UTF-8 sequence, so per-line lossy decoding
    /// matches whole-content decoding).
    fn push(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while let Some(position) = bytes[offset..].iter().position(|byte| *byte == b'\n') {
            let newline = offset + position;
            self.push_part(&bytes[offset..newline]);
            self.finish_line(false);
            offset = newline + 1;
        }
        self.push_part(&bytes[offset..]);
    }

    fn push_part(&mut self, bytes: &[u8]) {
        self.current_len += bytes.len();
        if !self.current_capped {
            let remaining = LINE_CAPTURE_BYTES.saturating_sub(self.current.len());
            if bytes.len() <= remaining {
                self.current.extend_from_slice(bytes);
            } else {
                self.current.extend_from_slice(&bytes[..remaining]);
                self.current_capped = true;
            }
        }
    }

    fn finish_line(&mut self, final_piece: bool) {
        // The synthetic final piece is the empty remainder after a trailing
        // newline (or the whole of an empty file). It is not a line, unlike a
        // real blank line terminated by '\n'.
        let synthetic_trailing_empty = final_piece && self.current_len == 0;
        let selected = self.line_index >= self.start_line && self.line_index < self.end_line;
        if selected {
            let text_len = if self.current_capped {
                // Over the capture cap: this line cannot fit the output
                // budget. For valid UTF-8 the byte length is exact; the BOM
                // (if present) is still stripped.
                let mut len = self.current_len;
                if self.line_index == 0 && self.current.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    len = len.saturating_sub(3);
                }
                len
            } else {
                decoded_line_len(&self.current, self.line_index)
            };
            let first = self.selected_lines == 0;
            self.selected_bytes += text_len + usize::from(!first);
            self.selected_lines += 1;
            // Only the synthetic final piece is outside the real line count.
            // A selected real blank line must still be counted.
            self.selected_last_empty = synthetic_trailing_empty;
            if first {
                self.first_line_len = text_len;
            }
            // The synthetic final empty piece still belongs to the content,
            // so it is retained (when the byte budget allows) to make
            // `join("\n")` reproduce the file's trailing newline. It is not a
            // counted line and never sets the byte-limit flag:
            // truncate_head's loop never visits that trailing separator, so
            // content that exceeds max_bytes only through it still reports
            // `truncatedBy: lines`.
            if !self.byte_limit_hit
                && (self.kept.len() < DEFAULT_MAX_LINES || synthetic_trailing_empty)
            {
                let line_bytes = text_len + usize::from(!self.kept.is_empty());
                if self.kept_bytes + line_bytes <= DEFAULT_MAX_BYTES && !self.current_capped {
                    self.kept.push(decoded_line(&self.current, self.line_index));
                    self.kept_bytes += line_bytes;
                    if !synthetic_trailing_empty {
                        self.kept_lines += 1;
                    }
                } else if !synthetic_trailing_empty {
                    self.byte_limit_hit = true;
                }
            }
        }
        self.line_index += 1;
        self.current.clear();
        self.current_len = 0;
        self.current_capped = false;
    }

    /// Finish the final (possibly empty) line piece and return the totals.
    fn finish(mut self) -> TextScanResult {
        self.finish_line(true);
        TextScanResult {
            // `line_index` counts the synthetic final piece exactly like
            // upstream's `text.split("\n")`: a trailing newline (and an empty
            // file) therefore contributes one line. Upstream uses that
            // `allLines.length` for the footer's "of N" and the offset bounds
            // check (`read.ts:157-164`), so both match here.
            total_file_lines: self.line_index,
            selected_lines: self.selected_lines,
            selected_bytes: self.selected_bytes,
            selected_last_empty: self.selected_last_empty,
            first_line_len: self.first_line_len,
            kept: self.kept,
            kept_bytes: self.kept_bytes,
            kept_lines: self.kept_lines,
            byte_limit_hit: self.byte_limit_hit,
        }
    }
}

struct TextScanResult {
    total_file_lines: usize,
    selected_lines: usize,
    selected_bytes: usize,
    selected_last_empty: bool,
    first_line_len: usize,
    kept: Vec<String>,
    kept_bytes: usize,
    kept_lines: usize,
    byte_limit_hit: bool,
}

/// Render the scanned result with the upstream truncation footers and details.
fn render_text(
    path: &str,
    raw_offset: Option<f64>,
    offset: usize,
    limit: Option<usize>,
    scan: TextScanResult,
) -> ToolResult {
    // Upstream read.ts: an out-of-bounds offset is an error. `total_file_lines`
    // counts the synthetic piece after a trailing newline (and the empty
    // file's single piece), so offset 1 on an empty file is valid and the
    // reported bound is `allLines.length`, exactly like upstream.
    let beyond_end = offset >= scan.total_file_lines;
    if beyond_end {
        return ToolResult::error(format!(
            "Offset {} is beyond end of file ({} lines total)",
            raw_offset.unwrap_or(1.0),
            scan.total_file_lines
        ));
    }
    let start_line_display = offset + 1;
    // `truncate_head`'s `split_lines` drops the trailing empty piece.
    let total_lines = scan
        .selected_lines
        .saturating_sub(usize::from(scan.selected_last_empty));
    let truncated = total_lines > DEFAULT_MAX_LINES || scan.selected_bytes > DEFAULT_MAX_BYTES;
    let mut details: Option<serde_json::Value> = None;
    let text: String;
    if truncated && scan.first_line_len > DEFAULT_MAX_BYTES {
        // First line alone exceeds the byte limit. Point the model at a bash
        // fallback (upstream `firstLineExceedsLimit`).
        let first_line_size = scan.first_line_len;
        text = format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(first_line_size),
            format_size(DEFAULT_MAX_BYTES),
            super::util::bash_quote(path),
        );
        let truncation = TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes: scan.selected_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            last_line_bytes: 0,
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        details = Some(serde_json::json!({ "truncation": truncation.to_details_json() }));
    } else if truncated {
        // Truncation occurred. Build an actionable continuation notice.
        // `kept` may contain the synthetic trailing empty piece so `join`
        // preserves a final newline; it is not a line and must not advance
        // the continuation offset.
        let output_lines = scan.kept_lines;
        let output_bytes = scan.kept_bytes;
        // `truncate_head` breaks at the first counted line that does not fit
        // the byte budget; when every counted line fits, the line limit is
        // what fired (even if the trailing newline pushed total_bytes over).
        let truncated_by = if scan.byte_limit_hit {
            TruncatedBy::Bytes
        } else {
            TruncatedBy::Lines
        };
        let end_line_display = start_line_display + output_lines.saturating_sub(1);
        let next_offset = end_line_display + 1;
        let content = scan.kept.join("\n");
        text = match truncated_by {
            TruncatedBy::Lines => format!(
                "{content}\n\n[Showing lines {start_line_display}-{end_line_display} of {}. Use offset={next_offset} to continue.]",
                scan.total_file_lines
            ),
            // Upstream's bytes footer reports the byte limit that fired.
            TruncatedBy::Bytes => format!(
                "{content}\n\n[Showing lines {start_line_display}-{end_line_display} of {} ({} limit). Use offset={next_offset} to continue.]",
                scan.total_file_lines,
                format_size(DEFAULT_MAX_BYTES),
            ),
        };
        let truncation = TruncationResult {
            content,
            truncated: true,
            truncated_by: Some(truncated_by),
            total_lines,
            total_bytes: scan.selected_bytes,
            output_lines,
            output_bytes,
            last_line_partial: false,
            last_line_bytes: 0,
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        details = Some(serde_json::json!({ "truncation": truncation.to_details_json() }));
    } else if let Some(user_limit) =
        limit.filter(|limit| offset.saturating_add(*limit) < scan.total_file_lines)
    {
        // User-specified limit stopped early, but the file still has more
        // content.
        let remaining = scan.total_file_lines - (offset + user_limit);
        let next_offset = offset + user_limit + 1;
        text = format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
            scan.kept.join("\n")
        );
    } else {
        // No truncation and no remaining user-limited content.
        text = scan.kept.join("\n");
    }
    let structured_content = serde_json::Value::String(text.clone());
    let mut result = ToolResult::ok(text);
    result.details = details;
    // Upstream `read.ts` always sets `structuredContent: toReadOutput(content)`
    // — a string for text reads.
    result.structured_content = Some(structured_content);
    result
}
