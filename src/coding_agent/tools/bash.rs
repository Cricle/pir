//! Bash tool: runs shell commands with interleaved stdout/stderr capture into
//! a bounded rolling buffer that spills oversized output to a temp file.
//!
//! Only [`BashTool`] is part of the `tool-bash` leaf feature (the `powershell`
//! leaf reuses the same spawn/capture machinery). The rest of this module — the
//! shell resolution ([`ShellConfig`], [`resolve_shell_config`],
//! [`apply_command_prefix`]), the process-tree kill handles, the pipe readers
//! and the rolling [`ShellCapture`] — is core: `agent_session::bash`, the
//! provider config-value executor, the TUI and the shell pool compile without
//! either tool.
#![cfg_attr(
    not(all(feature = "tool-bash", feature = "tool-powershell")),
    allow(dead_code)
)]

#[cfg(feature = "tool-bash")]
use crate::agent::tool_batch::ToolExecutionMode;
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::BoxFuture;
#[cfg(feature = "tool-bash")]
use crate::agent::tool_execution::{AgentTool, ToolContext};
use crate::coding_agent::truncate::{
    format_shell_output, truncate_tail, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

// ---------------------------------------------------------------------------
// Pluggable execution backend (upstream `BashOperations`, bash.ts:75-94)
// ---------------------------------------------------------------------------

/// Streamed stdout/stderr chunk callback (`on_data` in [`BashExecOptions`]).
pub type BashDataCallback = std::sync::Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Per-call execution options handed to a [`BashOperations`] backend.
///
/// `on_data` receives every stdout/stderr chunk as it arrives (the tool feeds
/// it into its own rolling capture, so truncation/footer/structured-content
/// shaping stays identical across backends). `env` carries only the session
/// variables the tool manages, with `None` meaning "remove this variable"
/// (upstream deletes then re-sets them at every spawn).
pub struct BashExecOptions {
    /// Streamed output chunks, stdout and stderr interleaved.
    pub on_data: BashDataCallback,
    /// Abort signal: the backend should terminate the command when it fires.
    pub signal: Option<crate::ai::types::AbortSignal>,
    /// Per-call timeout in seconds (upstream passes the raw value through).
    pub timeout: Option<f64>,
    /// PI_* session environment overrides (see [`super::bash::session_env_overrides`]).
    pub env: std::collections::BTreeMap<String, Option<String>>,
}

/// A pluggable command-execution backend (upstream `BashOperations`).
///
/// Override this to delegate command execution to a remote system (SSH, a
/// container, an in-process test double). `Ok(None)` reports the upstream
/// "terminated without an exit code" case; the two recognized error strings
/// are `"aborted"` and `"timeout:<seconds>"`, which the tool turns into the
/// same status lines the local shell produces.
pub trait BashOperations: Send + Sync {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a std::path::Path,
        options: BashExecOptions,
    ) -> BoxFuture<'a, Result<Option<i64>, String>>;
}

/// One spill file the capture owns: the write handle plus the path the tools
/// advertise. Dropping the guard without [`SpillHandle::advertised`] deletes the
/// file — a capture that ends up never handing its path out (an aborted run, a
/// latched spill failure that makes the file incomplete) must not leave a
/// secret-bearing temp file behind. An advertised file is kept for the model's
/// follow-up read and reaped later by the age/count/bytes sweep.
pub(crate) struct SpillHandle {
    file: super::spill::SpillFile,
    advertised: bool,
}

impl SpillHandle {
    /// Create a fresh restricted spill file (see `super::spill`).
    fn create() -> Option<Self> {
        let file = super::spill::create_spill_file()?;
        Some(Self {
            file,
            advertised: false,
        })
    }

    /// The capped writer (the per-file byte cap is enforced inside it).
    fn file_mut(&mut self) -> &mut super::spill::SpillFile {
        &mut self.file
    }

    fn path(&self) -> &std::path::Path {
        self.file.path()
    }

    fn path_string(&self) -> String {
        self.file.path().to_string_lossy().into_owned()
    }

    /// Keep the file past this run: the caller is about to hand its path to the
    /// model (`fullOutputPath` / `full_output_path`).
    fn advertised(mut self) -> String {
        self.advertised = true;
        self.path_string()
    }
}

impl Drop for SpillHandle {
    fn drop(&mut self) {
        if self.advertised {
            return;
        }
        // Best effort: the file is owned by this process and was never handed
        // out, so a failure to remove it only leaves a temp file the age sweep
        // collects later.
        let _ = std::fs::remove_file(self.file.path());
    }
}

/// Read at most `len` bytes at `offset` from `file` into `out` (`&File`
/// implements `Seek` + `Read`, so the bounded head/tail reads in
/// `ShellCapture::full_output` never materialize the whole spill file).
fn read_file_at(
    file: &std::fs::File,
    offset: u64,
    len: usize,
    out: &mut Vec<u8>,
) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut handle = file;
    handle.seek(SeekFrom::Start(offset))?;
    handle.take(len as u64).read_to_end(out)?;
    Ok(())
}

// 流式 shell 输出捕获：内存保留末尾 2×max_bytes 的滚动缓冲（涨到窗口的
// 2 倍时才裁剪回窗口，避免每个 chunk 都 memmove）；一旦裁剪，被挤出的前缀
// 全部落盘到临时文件（等价上游 OutputAccumulator 的 rolling buffer + spill
// 语义），截断时 footer 指向全量文件。
pub(crate) struct ShellCapture {
    tail: Vec<u8>,
    total: usize,
    /// 全量换行计数（上游 OutputAccumulator `completedLines`）：滚动窗口
    /// 只保留末尾，全量行数必须随 append 增量维护。
    completed_lines: usize,
    /// 末尾停留在未闭合行（上游 `hasOpenLine`）：totalLines =
    /// completed_lines + (has_open_line ? 1 : 0)。
    has_open_line: bool,
    keep: usize,
    spill: Option<SpillHandle>,
    /// The spill path once [`ShellCapture::spill_if_needed`] handed it out (the
    /// owning handle is released at that point so the file survives the
    /// capture; repeats of the call return this same path).
    advertised_path: Option<String>,
    /// Latched spill failure: spill-file creation or an evicted-prefix write
    /// failed once, so the file can no longer be the full stream and must not
    /// be advertised as `fullOutputPath`/`full_output_path` (advertising a
    /// truncated file would point the model at silent output loss). Creation
    /// is never retried per chunk once this is set.
    spill_failed: bool,
    /// The retained `tail` was appended to the spill file, so the file now
    /// holds the complete stream: a later eviction must not re-write the
    /// buffered head (`append` latches `spill_failed` instead).
    tail_flushed: bool,
    /// Instant of the last appended chunk (re-arms the post-exit stdio
    /// quiet window, extensions/runtime/exec.rs `StreamCapture.last_data`).
    last_data: std::time::Instant,
}

impl ShellCapture {
    pub(crate) fn new(keep: usize) -> Self {
        Self {
            tail: Vec::new(),
            total: 0,
            completed_lines: 0,
            has_open_line: false,
            keep,
            spill: None,
            advertised_path: None,
            spill_failed: false,
            tail_flushed: false,
            last_data: std::time::Instant::now(),
        }
    }

    /// The complete captured output (upstream `readFullOutput`): without a
    /// spill file the in-memory tail (no cap), else the file — plus the
    /// retained tail when it has not been flushed into the file yet (calls
    /// made before `spill_if_needed`). Past `max_bytes` the view keeps the
    /// first `max_bytes / 2` bytes and the final `max_bytes - head` bytes,
    /// both cut at UTF-8 boundaries, around an omission marker.
    ///
    /// The spill file holds the entire stream, so the reads are bounded
    /// exactly like upstream: `stat` the file, and for an oversized stream
    /// read only the head at offset 0 and the tail at `size - tail_bytes`
    /// instead of materializing every byte to keep `max_bytes`.
    pub(crate) fn full_output(&self, max_bytes: usize) -> (String, bool) {
        // A latched spill failure means the file may be missing an evicted
        // prefix: the in-memory tail is then incomplete and must be reported
        // as truncated instead of advertising dropped bytes as the full output.
        if self.spill_failed {
            return (self.tail_string(), true);
        }
        // No spill file (and no failure) means nothing was ever evicted, so
        // the tail is the complete stream. An advertised path keeps its file
        // readable after the owning handle was released.
        let path: &std::path::Path = match (self.spill.as_ref(), &self.advertised_path) {
            (Some(handle), _) => handle.path(),
            (None, Some(path)) => std::path::Path::new(path),
            (None, None) => return (self.tail_string(), false),
        };
        // Open a SEPARATE read handle (upstream `readFullOutput` opens the
        // temp path too): seeking the capture's write handle would move the
        // offset its later `spill_if_needed` append writes at.
        // A spill file that cannot be read leaves the evicted prefix
        // unavailable: the tail alone is incomplete, so report truncation.
        let Ok(file) = std::fs::File::open(path) else {
            return (self.tail_string(), true);
        };
        // The spill file holds the evicted prefix; the retained tail is only
        // in the file after `spill_if_needed` flushed it (the structured
        // builders run before that call), so the full stream is
        // `file ++ buffered_tail` until then.
        let buffered: &[u8] = if self.tail_flushed { &[] } else { &self.tail };
        let Ok(size) = file.metadata().map(|meta| meta.len() as usize) else {
            return (self.tail_string(), true);
        };
        let total = size.saturating_add(buffered.len());
        if total <= max_bytes {
            // Bounded by `max_bytes`: the whole-concatenation read stays at
            // or below the requested cap (upstream's `size <= maxBytes`
            // branch reads the file whole for the same reason).
            let mut bytes: Vec<u8> = Vec::with_capacity(total);
            if read_file_at(&file, 0, size, &mut bytes).is_err() {
                return (self.tail_string(), true);
            }
            bytes.extend_from_slice(buffered);
            return (String::from_utf8_lossy(&bytes).into_owned(), false);
        }
        let head_bytes = max_bytes / 2;
        let tail_bytes = max_bytes - head_bytes;
        // Head: the first `head_bytes` stream bytes plus ONE lookahead byte
        // for the boundary fixup (the stream is longer than `head_bytes`
        // here, so the lookahead is always available).
        let mut head: Vec<u8> = Vec::with_capacity(head_bytes + 1);
        let head_from_file = size.min(head_bytes + 1);
        if read_file_at(&file, 0, head_from_file, &mut head).is_err() {
            return (self.tail_string(), true);
        }
        if head.len() < head_bytes + 1 {
            // The file ended inside the head window: the rest is buffered.
            let from_tail = (head_bytes + 1 - head.len()).min(buffered.len());
            head.extend_from_slice(&buffered[..from_tail]);
        }
        // Tail: the final `tail_bytes` of the stream.
        let mut tail: Vec<u8> = Vec::with_capacity(tail_bytes);
        if buffered.len() >= tail_bytes {
            tail.extend_from_slice(&buffered[buffered.len() - tail_bytes..]);
        } else {
            let from_file = tail_bytes - buffered.len();
            let offset = size.saturating_sub(from_file);
            if read_file_at(&file, offset as u64, from_file, &mut tail).is_err() {
                return (self.tail_string(), true);
            }
            tail.extend_from_slice(buffered);
        }
        // UTF-8 boundary fixes (upstream streaming decoder + leading
        // continuation-byte skip): the head keeps whole characters, the tail
        // starts at a character start. `head` carries the byte at the cut
        // index, so the scan backs over an incomplete trailing sequence.
        let mut head_cut = head_bytes.min(head.len());
        while head_cut > 0 && head_cut < head.len() && (head[head_cut] & 0xC0) == 0x80 {
            head_cut -= 1;
        }
        let mut tail_start = 0;
        while tail_start < tail.len() && (tail[tail_start] & 0xC0) == 0x80 {
            tail_start += 1;
        }
        let head = String::from_utf8_lossy(&head[..head_cut]);
        let tail = String::from_utf8_lossy(&tail[tail_start..]);
        let omitted = total - head_bytes - tail_bytes;
        (
            format!("{head}\n\n[... {omitted} bytes omitted ...]\n\n{tail}"),
            true,
        )
    }

    pub(crate) fn append(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len());
        self.last_data = std::time::Instant::now();
        // 全量行计数（上游 appendDecodedText 的行语义）：换行累计 + 「末尾
        // 是否停在未闭合行」；空块不改变状态。'\n' 永不被分块切分，故
        // 「最后一个换行之后仍有内容」等价于块尾不是 '\n'。
        if !chunk.is_empty() {
            self.completed_lines = self
                .completed_lines
                .saturating_add(chunk.iter().filter(|&&byte| byte == b'\n').count());
            self.has_open_line = !chunk.ends_with(b"\n");
        }
        self.tail.extend_from_slice(chunk);
        // Trim only once the buffer has grown to twice the keep window: the
        // evicted range is then written in keep-sized blocks, so a steady
        // stream moves each retained byte O(1) times instead of memmoving the
        // whole window on every chunk past the cap (a 1 GB stream used to copy
        // roughly keep/chunk times the window: ~11 GB at the default keep).
        if self.tail.len() <= self.keep.saturating_mul(2) {
            return;
        }
        let mut excess = self.tail.len() - self.keep;
        {
            // Keep the retained tail starting at a character boundary. The
            // cut is otherwise a raw byte offset, and a multi-byte character
            // straddling it would begin `tail_string()` with a replacement
            // char (the same boundary rule `full_output` applies to its
            // head/tail cuts). A UTF-8 character has at most three
            // continuation bytes, so the cut moves back at most three bytes.
            let mut backed = 0;
            while excess < self.tail.len() && backed < 3 && (self.tail[excess] & 0xC0) == 0x80 {
                excess -= 1;
                backed += 1;
            }
            if excess == 0 {
                return;
            }
            if self.tail_flushed {
                // Defensive: the buffered head already went into the file
                // with the tail flush, so writing it again would duplicate
                // it. Real callers only finalize once the stream closed.
                self.spill_failed = true;
            } else {
                // Latch spill failures: a failed creation is never retried per
                // chunk, and a failed evicted-prefix write (disk full, the file
                // vanishing) marks the spill file incomplete so the result
                // builders stop advertising it as the full output.
                if self.spill.is_none() && !self.spill_failed {
                    match SpillHandle::create() {
                        Some(spill) => self.spill = Some(spill),
                        None => self.spill_failed = true,
                    }
                }
                if let Some(spill) = self.spill.as_mut() {
                    if spill.file_mut().write_all(&self.tail[..excess]).is_err() {
                        self.spill_failed = true;
                    }
                }
            }
            self.tail.drain(..excess);
        }
    }

    /// Ensure the spill file holds the COMPLETE decoded stream, then return
    /// its path (idempotent). With no file yet, `full_text` — the whole
    /// captured output — is written; an eviction spill is completed by
    /// appending the retained tail exactly once (upstream `ensureTempFile`
    /// flushes the buffered raw chunks). A latched spill failure (or a fresh
    /// failure here) yields `None` — an incomplete file must not be
    /// advertised as the full output.
    pub(crate) fn spill_if_needed(&mut self, full_text: &str) -> Option<String> {
        if self.spill_failed {
            return None;
        }
        if let Some(path) = &self.advertised_path {
            return Some(path.clone());
        }
        if self.spill.is_none() {
            let Some(mut spill) = SpillHandle::create() else {
                self.spill_failed = true;
                return None;
            };
            if spill.file_mut().write_all(full_text.as_bytes()).is_err() {
                self.spill_failed = true;
                return None;
            }
            self.spill = Some(spill);
            self.tail_flushed = true;
        } else if !self.tail_flushed {
            if let Some(spill) = self.spill.as_mut() {
                if spill.file_mut().write_all(&self.tail).is_err() {
                    self.spill_failed = true;
                    return None;
                }
            }
            self.tail_flushed = true;
        }
        // The path is about to be advertised, so the file must outlive the
        // capture: take the guard out and hand it over as an advertised file.
        let path = self.spill.take().map(SpillHandle::advertised)?;
        self.advertised_path = Some(path.clone());
        Some(path)
    }

    pub(crate) fn tail_string(&self) -> String {
        String::from_utf8_lossy(&self.tail).into_owned()
    }

    /// Borrowed form of [`Self::tail_string`]: the same lossy UTF-8 view, but
    /// a `Cow::Borrowed(&str)` when the captured tail is valid UTF-8 (the
    /// normal case). The streaming snapshot path runs on every throttled
    /// update / heartbeat, and copying the whole rolling tail there only to
    /// feed `truncate_tail` (which allocates its own retained window) was a
    /// pure per-tick allocation + memcpy.
    pub(crate) fn tail_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.tail)
    }

    /// The spill file's path, once the rolling buffer evicted a prefix AND
    /// no spill failure was latched (a failed spill must not be advertised).
    pub(crate) fn spill_path(&self) -> Option<String> {
        if self.spill_failed {
            return None;
        }
        match (&self.spill, &self.advertised_path) {
            (_, Some(path)) => Some(path.clone()),
            (Some(handle), None) => Some(handle.path_string()),
            (None, None) => None,
        }
    }

    /// The instant of the last appended chunk (the stdio quiet-window
    /// re-arm timestamp shared with the post-exit grace loop).
    pub(crate) fn last_chunk_at(&self) -> std::time::Instant {
        self.last_data
    }

    /// Overlay the incremental full-stream totals onto a window truncation
    /// (upstream `OutputAccumulator.snapshot`): `content` stays the retained
    /// tail, `totalLines`/`totalBytes` describe the whole stream. While
    /// nothing was evicted the tail IS the full stream, so its own counts
    /// already are the full counts and the result stays untouched (behavior
    /// below the window size is bit-identical); after an eviction only the
    /// incremental totals know the dropped prefix. The window's
    /// `truncated`/`truncatedBy` need no overlay: the window is a stream
    /// suffix, and the spill keep (`2 * DEFAULT_MAX_BYTES`) already exceeds
    /// `DEFAULT_MAX_BYTES`, so an evicted prefix always implies a truncated
    /// window too.
    pub(crate) fn with_full_totals(&self, mut truncation: TruncationResult) -> TruncationResult {
        if self.tail.len() == self.total {
            return truncation;
        }
        truncation.total_lines = self.completed_lines + usize::from(self.has_open_line);
        truncation.total_bytes = self.total;
        truncation
    }

    /// The throttled-update snapshot (upstream `OutputAccumulator.snapshot`):
    /// the tail view with truncation/full-path details. `None` until output
    /// arrived past `last_total` (which is advanced in place).
    pub(crate) fn update_snapshot(&self, last_total: &mut usize) -> Option<serde_json::Value> {
        if self.total == *last_total {
            return None;
        }
        *last_total = self.total;
        let view = self.with_full_totals(truncate_tail(
            // Borrow the rolling tail (Cow) instead of `tail_string()`'s full
            // copy: this runs per throttled update while a command streams.
            &self.tail_lossy(),
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES,
        ));
        let mut details = serde_json::Map::new();
        if view.truncated {
            details.insert("truncation".to_owned(), view.to_details_json());
            if let Some(path) = self.spill_path() {
                details.insert("fullOutputPath".to_owned(), serde_json::json!(path));
            }
        }
        let details = if details.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details))
        };
        Some(shell_update_payload(&view.content, details))
    }
}

// ---------------------------------------------------------------------------
// Bash tool
// ---------------------------------------------------------------------------

/// Upstream bash.ts MAX_TIMEOUT_MS: the timeout cap in milliseconds.
const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// Pipe-quiet grace after the shell child exits (or the timeout/abort kill
/// fired) on the per-call spawn path (upstream `waitForChildProcess`,
/// `utils/child-process.ts:88-97`, via extensions/runtime/exec.rs): once the
/// child is gone the pipe readers must not be joined indefinitely — a
/// detached grandchild that inherited the stdout/stderr write ends (e.g.
/// `python -m http.server &`) would block a plain join forever. The window
/// re-arms on every chunk so a still-writing descendant is drained; when it
/// expires the readers are dropped (closing the read ends) and whatever was
/// captured proceeds. pir takes a larger margin than upstream's 100 ms for
/// its 50 ms process-poll granularity.
pub(crate) const EXIT_STDIO_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Absolute wall-clock cap on the post-exit pipe drain. The quiet window above
/// re-arms on every captured chunk (upstream `waitForChildProcess` fidelity),
/// so a chatty descendant that inherited the stdout/stderr write ends would
/// otherwise keep the call alive past the command timeout forever. Ten quiet
/// windows leave ample time for the readers to deliver the command's own tail
/// even on a loaded machine while still bounding a chatty descendant.
pub(crate) const EXIT_STDIO_ABSOLUTE_CAP: std::time::Duration = std::time::Duration::from_secs(5);
/// Upstream bash.ts MAX_TIMEOUT_SECONDS = MAX_TIMEOUT_MS / 1000.
const MAX_TIMEOUT_SECONDS: i64 = 2_147_483;
/// Output limit of `structuredContent.output` (upstream
/// STRUCTURED_OUTPUT_MAX_BYTES): programmatic callers such as codemode
/// scripts decide how much of it reaches the model.
pub(crate) const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

/// The settings key that supplies the default command timeout. Named verbatim
/// in the timeout error so the model (and the user reading the transcript) can
/// find the setting that killed the command.
pub(crate) const COMMAND_TIMEOUT_SETTING: &str = "tools.commandTimeoutMs";

/// Resolve the effective shell-command timeout. An explicit per-call
/// `timeout` always wins (upstream semantics, `None` included); otherwise
/// `tools.commandTimeoutMs` applies — 120 s by default, a deliberate
/// deviation from upstream, disabled by an explicit `0`. The returned bool is
/// true when the cap came from the setting, so the timeout status line can
/// name it instead of pretending the caller passed a timeout.
/// `(None, false)` means the caller disabled the cap.
pub(crate) fn resolve_command_timeout(
    call_timeout_secs: Option<f64>,
    setting_timeout: Option<std::time::Duration>,
) -> (Option<f64>, bool) {
    if let Some(secs) = call_timeout_secs {
        return (Some(secs), false);
    }
    match setting_timeout {
        Some(duration) => (
            Some((duration.as_secs_f64()).min(MAX_TIMEOUT_SECONDS as f64)),
            true,
        ),
        None => (None, false),
    }
}

/// The model-facing timeout status line. A settings-imposed cap names the
/// setting that fired (and how to turn it off), so the error is honest about
/// where the deadline came from; an explicit per-call `timeout` keeps
/// upstream's exact wording.
pub(crate) fn format_command_timed_out(
    timeout_secs: Option<f64>,
    timeout_from_setting: bool,
) -> String {
    let seconds = timeout_secs.unwrap_or(0.0);
    if timeout_from_setting {
        format!(
            "Command timed out after {seconds} seconds (default command timeout from {COMMAND_TIMEOUT_SETTING}; pass a larger per-call `timeout`, raise that setting, or set it to 0 to disable the default)"
        )
    } else {
        format!("Command timed out after {seconds} seconds")
    }
}

/// Upstream `resolveTimeoutMs` in seconds form: `None` for absent, an error
/// string for invalid values, `Some(seconds)` otherwise.
pub(crate) fn validate_timeout_secs(timeout: Option<f64>) -> Result<Option<f64>, String> {
    match timeout {
        None => Ok(None),
        Some(secs) if !secs.is_finite() || secs <= 0.0 => {
            Err("Invalid timeout: must be a finite number of seconds".to_owned())
        }
        Some(secs) if secs * 1000.0 > MAX_TIMEOUT_MS => Err(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        )),
        Some(secs) => Ok(Some(secs)),
    }
}

#[cfg(feature = "tool-bash")]
pub struct BashTool {
    operations: Option<std::sync::Arc<dyn BashOperations>>,
}

#[cfg(feature = "tool-bash")]
impl BashTool {
    /// The built-in tool: local shell (or the `env_client` remote arm when
    /// configured).
    pub fn new() -> Self {
        Self { operations: None }
    }

    /// The tool with an explicit execution backend (upstream
    /// `createBashTool(cwd, { operations })`). An injected backend wins over
    /// the process-wide `env_client` selection.
    pub fn with_operations(operations: std::sync::Arc<dyn BashOperations>) -> Self {
        Self {
            operations: Some(operations),
        }
    }
}

#[cfg(feature = "tool-bash")]
impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The effective shell configuration (upstream `ShellConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    program: String,
    args: Vec<&'static str>,
    /// Legacy WSL bash (`C:\Windows\...\bash.exe`) transports the command on
    /// stdin instead of `-c` (upstream `getBashShellConfig`).
    stdin_transport: bool,
}

impl ShellConfig {
    /// The embedded shell (`pir __brush`) in bash's `-c <script>` shape: the
    /// command is appended as the next argument after `args`, exactly like the
    /// external bash. `None` when the executable path cannot be resolved.
    #[cfg(feature = "brush")]
    fn embedded() -> Option<Self> {
        crate::shell_embed::embedded_shell_program().map(|program| Self {
            program,
            args: vec!["__brush", "-c"],
            stdin_transport: false,
        })
    }

    /// The shell executable to spawn.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The fixed arguments passed before the command (`-c`), or alone when the
    /// command travels on stdin (`-s`).
    pub fn args(&self) -> &[&'static str] {
        &self.args
    }

    /// Whether the command must be written to the child's stdin rather than
    /// appended to `args` (upstream `commandTransport === "stdin"`).
    pub fn stdin_transport(&self) -> bool {
        self.stdin_transport
    }

    /// A shell config for a KNOWN program and fixed args (the `cmd /C` /
    /// `sh -c` fallback in [`crate::ai::providers::config_value`], which must
    /// mirror `execSync`'s default shell without re-running discovery).
    pub fn explicit(program: &str, args: Vec<&'static str>, stdin_transport: bool) -> Self {
        Self {
            program: program.to_string(),
            args,
            stdin_transport,
        }
    }

    /// Spawn this shell to run `command`, returning the child with stdout
    /// piped and stdin/stderr as upstream's
    /// `stdio: [commandFromStdin ? "pipe" : "ignore", "pipe", "ignore"]`
    /// (`resolve-config-value.ts:161,190`).
    ///
    /// Used by the config-value executor, which needs the same shell choice
    /// the bash tool makes but with a bounded blocking wait instead of the
    /// async tree-kill path. A stdin-transport shell (legacy WSL bash) gets
    /// the command on stdin, so the caller must write it there and close it.
    pub fn spawn(&self, command: &str) -> std::io::Result<std::process::Child> {
        let mut spawn = std::process::Command::new(&self.program);
        spawn.args(&self.args);
        if self.stdin_transport {
            spawn.stdin(std::process::Stdio::piped());
        } else {
            spawn.arg(command);
            spawn.stdin(std::process::Stdio::null());
        }
        spawn.stdout(std::process::Stdio::piped());
        spawn.stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW: a config-value command must not flash a
            // console window (upstream `windowsHide: true`, :163).
            use std::os::windows::process::CommandExt;
            spawn.creation_flags(0x0800_0000);
        }
        spawn.spawn()
    }
}

/// Upstream `isLegacyWslBashPath`: `^[a-z]:\windows\(system32|sysnative)\bash.exe$`
/// (case-insensitive, forward slashes and `\\?\` verbatim prefixes normalized
/// away — a `\\?\C:\Windows\System32\bash.exe` setting must still resolve to
/// the stdin transport).
fn is_legacy_wsl_bash_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_lowercase();
    let normalized = normalized.strip_prefix(r"\\?\").unwrap_or(&normalized);
    let Some(rest) = normalized.strip_prefix(|c: char| c.is_ascii_alphabetic()) else {
        return false;
    };
    // The drive letter is followed by `:\windows\...`.
    let Some(rest) = rest.strip_prefix(":\\windows\\") else {
        return false;
    };
    rest == "system32\\bash.exe" || rest == "sysnative\\bash.exe"
}

/// Upstream `getBashShellConfig`: legacy WSL bash takes `-s` and the command
/// on stdin; every other shell takes `-c` with the command appended.
fn bash_shell_config(shell: &str) -> ShellConfig {
    if is_legacy_wsl_bash_path(shell) {
        ShellConfig {
            program: shell.to_string(),
            args: vec!["-s"],
            stdin_transport: true,
        }
    } else {
        ShellConfig {
            program: shell.to_string(),
            args: vec!["-c"],
            stdin_transport: false,
        }
    }
}

/// Locate an executable on `PATH` (upstream `findExecutableOnPath`: `where` on
/// Windows, `which` elsewhere; the first existing match wins).
fn find_executable_on_path(executable: &str) -> Option<String> {
    let (probe, arg) = if cfg!(windows) {
        ("where", executable)
    } else {
        ("which", executable)
    };
    let output = std::process::Command::new(probe)
        .arg(arg)
        // Rust's default stdin is inherit (Node's spawn default is a pipe):
        // pin it null so the probe never reads the host's stdin.
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && std::path::Path::new(line).exists())
        .map(|line| line.to_string())
}

/// Resolve the shell to use (upstream `getShellConfig`):
/// 1. the settings `shellPath` when it exists (else an error);
/// 2. Windows: Git Bash under `%ProgramFiles%` / `%ProgramFiles(x86)%` /
///    `%LOCALAPPDATA%\Programs` (per-user install), then `bash.exe` on `PATH`;
///    Unix: `/bin/bash`, then `bash` on `PATH`;
/// 3. with the `brush` feature, the embedded shell (`pir __brush -c`) as the
///    fallback when no external bash exists — so no install is required;
/// 4. Unix: `sh` as the last resort.
///
/// An installed Git Bash wins over the embedded shell: it carries the full
/// coreutils/PATH surface (`head`, `tail`, `date`, …) that scripts and the
/// model expect, matching upstream pi. The embedded shell remains the
/// zero-install fallback.
pub fn resolve_shell_config(shell_path: Option<&std::path::Path>) -> Result<ShellConfig, String> {
    if let Some(path) = shell_path {
        if !path.exists() {
            return Err(format!("Custom shell path not found: {}", path.display()));
        }
        return Ok(bash_shell_config(&path.to_string_lossy()));
    }

    if cfg!(windows) {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(program_files) = std::env::var_os(variable) {
                candidates.push(
                    std::path::PathBuf::from(program_files)
                        .join("Git")
                        .join("bin")
                        .join("bash.exe"),
                );
            }
        }
        // Per-user Git for Windows installs (the installer's default when it
        // cannot write Program Files) land here; they are not on PATH unless
        // the user opted in, so seed the same probe before failing closed.
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(
                std::path::PathBuf::from(local_app_data)
                    .join("Programs")
                    .join("Git")
                    .join("bin")
                    .join("bash.exe"),
            );
        }
        for candidate in &candidates {
            if candidate.exists() {
                return Ok(bash_shell_config(&candidate.to_string_lossy()));
            }
        }
        if let Some(bash) = find_executable_on_path("bash.exe") {
            return Ok(bash_shell_config(&bash));
        }
        #[cfg(feature = "brush")]
        if let Some(embedded) = ShellConfig::embedded() {
            return Ok(embedded);
        }
        let searched = candidates
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: \
             https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  \
             3. Set shellPath in settings.json\n\nSearched Git Bash in:\n{searched}"
        ));
    }

    if std::path::Path::new("/bin/bash").exists() {
        return Ok(bash_shell_config("/bin/bash"));
    }
    if let Some(bash) = find_executable_on_path("bash") {
        return Ok(bash_shell_config(&bash));
    }
    #[cfg(feature = "brush")]
    if let Some(embedded) = ShellConfig::embedded() {
        return Ok(embedded);
    }
    Ok(ShellConfig {
        program: "sh".to_string(),
        args: vec!["-c"],
        stdin_transport: false,
    })
}

/// Apply the settings `shellCommandPrefix` (upstream bash tool
/// `resolvedCommand = commandPrefix ? \`${commandPrefix}\n${command}\` : command`).
pub fn apply_command_prefix(prefix: Option<&str>, command: &str) -> String {
    match prefix {
        Some(prefix) if !prefix.trim().is_empty() => format!("{prefix}\n{command}"),
        _ => command.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Process-tree termination (upstream utils/shell.ts `killProcessTree`)
// ---------------------------------------------------------------------------

/// Windows Job Object wrapper for a spawned shell child: the child and every
/// descendant run inside the job, so a job-wide `TerminateJobObject` reaches
/// the whole tree (upstream `killProcessTree` uses `taskkill /T`; the job
/// avoids spawning a helper and cannot miss fast-reparenting grandchildren).
/// The job carries NO kill-on-close limit: dropping the handle at the end of
/// a completed run leaves intentionally backgrounded descendants running,
/// exactly like upstream, which only kills on abort/timeout.
#[cfg(windows)]
pub(crate) struct JobHandle {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// The Job Object HANDLE is a plain kernel handle usable from any thread.
// windows-sys 0.52 spells `HANDLE` as `isize`, so `Send`/`Sync` are derived
// structurally and no manual (unsafe) impl is needed. The `Arc` that owns it
// keeps it open until the detached tree-kill sweep is done, so sending the
// handle to the blocking thread is sound.
#[cfg(windows)]
const _: fn() = {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<JobHandle>
};

/// One failed Job Object step, with the Win32 error that caused it (the
/// degradation to the sweeps must be diagnosable from the log: `GetLastError`
/// tells an access-denied refusal from a bad-length setup bug).
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct JobSetupError {
    /// Which step failed.
    step: &'static str,
    /// `GetLastError()` of that step (`0` when the process handle was
    /// missing, i.e. no Win32 call was involved).
    code: i32,
}

#[cfg(windows)]
impl std::fmt::Display for JobSetupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} failed (win32 error {})",
            self.step, self.code
        )
    }
}

#[cfg(windows)]
impl JobHandle {
    /// Create a Job Object and move the child process into it. `Err` when the
    /// OS setup fails at any step (the caller then degrades to the
    /// `taskkill`/MSYS sweeps and logs the step plus its Win32 error).
    /// `kill_on_close` arms `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — used ONLY
    /// for pooled members (the persistent shell is pool plumbing, not a
    /// user-backgrounded descendant, and must die with the process); the
    /// per-call spawn path keeps `false` so a completed run leaves
    /// intentionally backgrounded descendants alive (upstream semantics).
    fn assign(
        process: std::os::windows::io::RawHandle,
        kill_on_close: bool,
    ) -> Result<Self, JobSetupError> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        #[expect(
            unsafe_code,
            reason = "Win32 Job Object setup for a child process we spawned"
        )]
        // SAFETY: the calls operate on handles created/borrowed in this block:
        // the fresh Job Object handle and the caller's live child process
        // handle. Every failure path closes the job handle before returning.
        // windows-sys 0.52 types HANDLE as `isize`: a null job handle is 0.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return Err(JobSetupError {
                    step: "CreateJobObjectW",
                    code: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
                });
            }
            if kill_on_close {
                // KILL_ON_JOB_CLOSE must be set through the EXTENDED limit
                // info: `SetInformationJobObject(JobObjectBasicLimitInformation)`
                // rejects the flag with ERROR_INVALID_PARAMETER (87), which is
                // exactly what every pooled-member setup logged. The extended
                // struct embeds the basic one, so the flag keeps its meaning;
                // windows-sys 0.52 structs carry no Default derive and the
                // info class is an i32 alias constant, not an enum variant.
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    let _ = CloseHandle(job);
                    return Err(JobSetupError {
                        step: "SetInformationJobObject(KILL_ON_JOB_CLOSE)",
                        code,
                    });
                }
            }
            if AssignProcessToJobObject(job, process as _) != 0 {
                return Ok(Self { handle: job });
            }
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            let _ = CloseHandle(job);
            Err(JobSetupError {
                step: "AssignProcessToJobObject",
                code,
            })
        }
    }

    /// Terminate every process in the job. `Err` with the Win32 error when the
    /// OS call failed (the caller then falls back to the sweeps).
    fn terminate(&self) -> Result<(), i32> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        #[expect(
            unsafe_code,
            reason = "Win32 job-wide termination for a shell child we spawned"
        )]
        // SAFETY: `self.handle` is the live Job Object handle owned by this
        // value (closed only in `Drop`); `TerminateJobObject` reads no other
        // memory.
        unsafe {
            if TerminateJobObject(self.handle, 1) != 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            }
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        #[expect(unsafe_code, reason = "close the Job Object handle we created")]
        // SAFETY: `self.handle` was created by `JobHandle::assign` and is owned
        // by this value; `Drop` runs exactly once, so the handle is closed
        // exactly once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Armed per-run tree-kill handle for the shell tools.
pub(crate) struct ProcessTreeKill {
    #[cfg(windows)]
    job: Option<std::sync::Arc<JobHandle>>,
}

impl ProcessTreeKill {
    /// Arm the tree kill for `child`: on Windows the child is moved into a
    /// Job Object (`job: None` → `taskkill /T /F` fallback), on Unix the
    /// child was spawned as its own process-group leader (see
    /// `spawn_shell`), so a group-wide signal reaches the whole tree.
    pub(crate) fn attach(child: &tokio::process::Child) -> Self {
        Self::attach_with(child, false)
    }

    /// [`ProcessTreeKill::attach`] with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    /// for pooled members: the persistent shell is pool plumbing that must
    /// die with the process, not a user-backgrounded descendant.
    pub(crate) fn attach_for_pool(child: &tokio::process::Child) -> Self {
        Self::attach_with(child, true)
    }

    fn attach_with(child: &tokio::process::Child, kill_on_close: bool) -> Self {
        #[cfg(windows)]
        {
            // Platform-inherent race (same window upstream's `taskkill /T`
            // has): the child already exists, so anything it spawns between
            // CreateProcess and this AssignProcessToJobObject escapes the
            // job. The `taskkill /T /F` sweep in [`ProcessTreeKill::terminate`]
            // still reaches stragglers whose Windows parent link is intact.
            let pid = child.id().unwrap_or_default();
            let job = match child.raw_handle() {
                Some(handle) => match JobHandle::assign(handle, kill_on_close) {
                    Ok(job) => Some(std::sync::Arc::new(job)),
                    Err(error) => {
                        // A leaky fallback must not be invisible: without a job
                        // the tree kill degrades to the `taskkill`/MSYS sweeps,
                        // and the step plus its Win32 error are what make the
                        // degradation diagnosable (an `AssignProcessToJobObject`
                        // access-denied refusal in a job-hostile host — ConPTY
                        // hosted trees — looks different from a setup bug).
                        tracing::warn!(
                            target: "pir::shell_pool",
                            pid,
                            error = %error,
                            "shell tree kill: job object setup failed; the tree kill degrades to the taskkill/MSYS sweeps"
                        );
                        None
                    }
                },
                None => {
                    tracing::warn!(
                        target: "pir::shell_pool",
                        pid,
                        "shell tree kill: the child process handle is unavailable; the tree kill degrades to the taskkill/MSYS sweeps"
                    );
                    None
                }
            };
            Self { job }
        }
        #[cfg(not(windows))]
        {
            let _ = (child, kill_on_close);
            Self {}
        }
    }

    /// Kill the shell's whole descendant tree (upstream `killProcessTree`).
    ///
    /// Windows runs every step unconditionally, in this order (the caller's
    /// `start_kill` still covers the root; Unix is unchanged):
    /// 1. the MSYS-side sweep ([`msys_subtree_kill`]) FIRST, while the root is
    ///    still alive and its MSYS process-table entry intact;
    /// 2. the trusted `taskkill /F /T /PID <pid>` sweep, which covers roots
    ///    that are not MSYS processes (e.g. the PowerShell shell);
    /// 3. the Job Object, when the child was moved into one.
    ///
    /// Why a successful job terminate must NOT skip step 1: measured on
    /// Windows, an MSYS `bash` fork records a short-lived helper's pid as the
    /// forked child's Windows parent (the descendant's PPID is a process that
    /// is already gone), and in the failing environment the fork also escapes
    /// Job Object membership — a successful `TerminateJobObject` on the root's
    /// job still left the forked descendant alive. Observed survivor shape:
    /// the shell and its direct child die, exactly the forked background
    /// descendant (`sleep 30 &` inside the command) stays, for both the
    /// taskkill sweep and the job terminate. Only MSYS's own (correct) parent
    /// links reach it, so the resolution runs before anything kills the root.
    /// (An isolated probe where the forked children did inherit the job reaped
    /// all three, which is why the job remains the third step rather than a
    /// replacement.)
    ///
    /// Synchronous on purpose: the MSYS sweep needs the ROOT's process-table
    /// entry, so it must complete before the caller's `start_kill`. An async
    /// offload (detached thread) was tried and reverted — it raced
    /// `start_kill`, the sweep found no root row, and descendants survived
    /// (regression test:
    /// `background_tool_calls::abort_background_kills_the_detached_bash_process`).
    /// Consequence: the sweeps spawn helper processes and block until they
    /// exit, so a kill can take 150-300 ms idle and seconds under load on the
    /// abort/timeout path (measured 4.2 s with parallel test load). The shell
    /// config is cached, so a kill does not re-read settings/trust.
    pub(crate) fn terminate(&self, pid: u32) {
        #[cfg(windows)]
        {
            msys_subtree_kill(pid);
            taskkill_tree(pid);
            // A job that exists but cannot be terminated ends up with only the
            // sweeps above; that must not be silent (same reason as the
            // attach-time warn), and the Win32 error tells the failure modes
            // apart.
            if let Some(job) = self.job.as_ref() {
                if let Err(code) = job.terminate() {
                    tracing::warn!(
                        target: "pir::shell_pool",
                        pid,
                        win32_error = code,
                        "shell tree kill: TerminateJobObject failed; the tree kill degraded to the sweeps"
                    );
                }
            }
        }
        #[cfg(unix)]
        {
            // The child leads its own process group (spawned with
            // `process_group(0)`), so a group-wide SIGKILL reaches the whole
            // tree (upstream `process.kill(-pid, "SIGKILL")`); ESRCH from a
            // group that is already gone falls back to the direct kill.
            #[expect(
                unsafe_code,
                reason = "libc::killpg on the shell child's own process group"
            )]
            // SAFETY: `pid` is the shell child spawned as its own process-group
            // leader; both calls take plain integer ids and no pointers. A
            // failed group signal falls back to the direct child pid, which is
            // equally valid for this process.
            unsafe {
                if libc::killpg(pid as libc::pid_t, libc::SIGKILL) != 0 {
                    let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = pid;
        }
    }
}

/// The trusted System32 `taskkill /T /F` sweep (best effort, upstream
/// `killProcessTree`): kills the process and every child that still carries a
/// Windows parent link. No console window flashes.
#[cfg(windows)]
fn taskkill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
    let _ = std::process::Command::new(
        std::path::Path::new(&system_root)
            .join("System32")
            .join("taskkill.exe"),
    )
    .args(["/F", "/T", "/PID", &pid.to_string()])
    .env("NoDefaultCurrentDirectoryInExePath", "1")
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    // CREATE_NO_WINDOW (0x08000000): the kill helper must not flash a console
    // (upstream `windowsHide: true`).
    .creation_flags(0x0800_0000)
    .status();
}

/// RAII guard around one per-call shell's process-tree kill.
///
/// The explicit kill below already runs on abort/timeout, but a tool call can
/// also leave the wait loop without passing through that code: the caller
/// cancels the future (the TUI aborts its prompt task, the session aborts a
/// background call), a panic unwinds, or an early `return` (a missing pipe)
/// skips it. `kill_on_drop(true)` only reaches the shell itself — the
/// command's descendants (a forked `sleep`, a started server) would survive.
/// The guard arms once at spawn, kills the whole tree from `Drop` while it is
/// still armed, and is disarmed as soon as the run reaches its own teardown
/// (normal exit or explicit kill).
///
/// The kill is synchronous by necessity (see `ProcessTreeKill::terminate`:
/// the MSYS sweep needs the root's process-table entry, so it must complete
/// before the root is reaped). Drop therefore blocks the dropping thread for
/// the same 150-300 ms the abort path already accepts, and only on the
/// abnormal path.
pub(crate) struct TreeKillGuard {
    /// `Some((kill handle, root pid))` while the tree may still be running.
    armed: Option<(ProcessTreeKill, u32)>,
}

impl TreeKillGuard {
    /// Arm for a freshly spawned child. The pid is captured here because
    /// `Child::id` returns `None` once the child has been reaped.
    pub(crate) fn arm(child: &tokio::process::Child) -> Self {
        let pid = child.id();
        let kill = ProcessTreeKill::attach(child);
        Self {
            armed: pid.map(|pid| (kill, pid)),
        }
    }

    /// Kill the tree now and disarm (the explicit abort/timeout path).
    pub(crate) fn terminate(&mut self) {
        if let Some((kill, pid)) = self.armed.take() {
            kill.terminate(pid);
        }
    }

    /// The child exited on its own: intentionally backgrounded descendants
    /// keep running (upstream semantics), so the guard must not fire.
    pub(crate) fn disarm(&mut self) {
        self.armed = None;
    }
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        if let Some((kill, pid)) = self.armed.take() {
            kill.terminate(pid);
        }
    }
}

/// The shell used by the MSYS tree sweep, resolved once on first use.
///
/// `current_shell_prefs()` reads settings (and the trust store) from disk;
/// resolving it again on every abort/timeout is wasted I/O on a path that
/// already spawns helpers. The cached value matches the process-lifetime
/// settings snapshot the shell tools otherwise resolve per call; a settings
/// change after the first kill only affects this best-effort sweep, not the
/// command's actual shell (which is resolved per spawn).
#[cfg(windows)]
static MSYS_SWEEP_SHELL: std::sync::OnceLock<Option<ShellConfig>> = std::sync::OnceLock::new();

#[cfg(windows)]
fn msys_sweep_shell() -> Option<&'static ShellConfig> {
    MSYS_SWEEP_SHELL
        .get_or_init(|| {
            let prefs = crate::config::settings::current_shell_prefs();
            resolve_shell_config(prefs.shell_path.as_deref())
                // The embedded shell has no MSYS process table: the sweep only
                // applies to an external MSYS bash, so it simply does not run
                // when the command itself ran in the embedded shell.
                .ok()
                .filter(|shell| !shell.args().contains(&"__brush"))
        })
        .as_ref()
}

/// The MSYS-side descendant sweep through the MSYS runtime's own process
/// table — the first step of [`ProcessTreeKill::terminate`], always run.
///
/// Windows is the authority for neither MSYS pids nor MSYS parent links: a
/// forked background descendant keeps a Windows PPID whose process is already
/// gone (measured) and escapes a Job Object assigned to the root, so neither a
/// Windows tree walk nor `TerminateJobObject` reaches it (see
/// [`ProcessTreeKill::terminate`] for the observed survivor shape). MSYS keeps
/// its own, correct parent links, so an MSYS shell maps the root's Windows pid
/// to its MSYS pid (`ps -W`'s WINPID column), collects the MSYS subtree hanging
/// off it and `kill -9`s it — subtree-scoped, so unrelated MSYS processes
/// (including the helper running this) are untouched. It must run while the
/// root is still alive: the walk starts at the root's own table entry.
///
/// Best effort by design: no resolvable shell (or no matching row, e.g. a
/// PowerShell root) means the sweep simply does not run — the `taskkill` sweep
/// and the caller's `start_kill` still cover the root.
#[cfg(windows)]
fn msys_subtree_kill(root_win_pid: u32) {
    // Resolve the same shell the command ran in (`shellPath` included): the
    // sweep reads that installation's process table, so a different shell
    // would simply find no matching row and skip.
    let Some(shell) = msys_sweep_shell() else {
        return;
    };
    let script = format!(
        r#"win="{root_win_pid}"
pids=$(ps -W 2>/dev/null | awk -v w="$win" '
  {{ pid=$1; ppid=$2; winpid=$4; parent[pid]=ppid; if (winpid == w) root=pid }}
  END {{
    if (root == "") exit 3
    n=0; stack[++n]=root; out=""
    while (n > 0) {{ p=stack[n--]; out=out " " p;
      for (q in parent) if (parent[q] == p) stack[++n]=q }}
    print out
  }}')
[ -n "$pids" ] || exit 4
kill -9 $pids 2>/dev/null || exit 5
"#
    );
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new(shell.program());
    command
        .args(shell.args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000);
    if shell.stdin_transport() {
        // Legacy WSL bash reads the script on stdin (`-s`).
        use std::io::Write;
        command.stdin(std::process::Stdio::piped());
        let Ok(mut child) = command.spawn() else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        let _ = child.wait();
        return;
    }
    let _ = command.arg(&script).status();
}

/// The program a long-lived shell process must be spawned with, plus the
/// `PATH` value that spawn needs, when the resolved shell is a
/// Git-for-Windows *shim* rather than the real MSYS bash.
///
/// `C:\Program Files\Git\bin\bash.exe` is a 47 KB native launcher that runs
/// `<root>\usr\bin\bash.exe` as a CHILD and only fixes up `PATH` before doing
/// so. A process spawned through it is therefore not an MSYS process at all,
/// which was measured to defeat every tree-kill mechanism at once:
/// * `ps -W` classifies the shim with a synthetic pid (`0x400000 + winpid`,
///   `PPID 0`) and no MSYS identity, so the MSYS subtree sweep cannot resolve
///   the root (the command's descendants are unreachable);
/// * the children it hosts demonstrably escape a Job Object assigned to the
///   shim (`TerminateJobObject` left the command's subshell and forked `sleep`
///   alive), whereas a real-MSY-bash root's children inherit the job and all
///   die;
/// * the forked `sleep` keeps a Windows PPID whose process is already gone,
///   so `taskkill /T` misses it as well.
///
/// Spawning the real bash directly gives a member that is a real MSYS process
/// (WINPID == the pid we hold) and whose descendants inherit the job. The one
/// environment fixup the shim performs is replicated here: it prepends
/// `/mingw64/bin:/usr/bin` to `PATH` (measured; with it, tool resolution is
/// identical to the shim for find/sort/tar/grep/sed/awk/env/ls/python and
/// every other environment variable matches).
///
/// `None` for every other shell (an MSYS2 install whose bash is already the
/// real one, a Linux bash, the WSL launcher, …): those keep their behavior
/// byte-identical.
#[cfg(windows)]
pub(crate) fn msys_shim_spawn_target(shell_program: &str) -> Option<(String, String)> {
    let program = std::path::Path::new(shell_program);
    // `<root>/bin/bash.exe` with a real `<root>/usr/bin/bash.exe` next to it.
    let root = program.parent()?.parent()?;
    let real = root.join("usr").join("bin").join("bash.exe");
    if !real.is_file() {
        return None;
    }
    // Only a *launcher* is smaller than the bash it launches (measured: the
    // Git-for-Windows shim is 47 KB against the real 2.4 MB bash). A layout
    // where `bin/bash.exe` is the real binary again (a copy, or MSYS2's `bin`
    // junction onto `usr/bin`) is left completely untouched: same file, same
    // behavior, no PATH fixup needed.
    let launcher_size = std::fs::metadata(program).ok()?.len();
    let real_size = std::fs::metadata(&real).ok()?.len();
    if launcher_size >= real_size {
        return None;
    }
    let path = std::env::var("PATH").unwrap_or_default();
    Some((
        real.to_string_lossy().into_owned(),
        format!("/mingw64/bin:/usr/bin:{path}"),
    ))
}

/// The spawn target every shell spawn site must use: the resolved program, or
/// the real MSYS bash when the resolved one is a Git-for-Windows launcher shim
/// ([`msys_shim_spawn_target`]), plus the `PATH` value that spawn needs.
///
/// `None` means "spawn the program as-is" — every non-shim shell (Linux bash,
/// MSYS2's own bash, the WSL launcher, PowerShell) keeps its spawn
/// byte-identical. The shim case must be applied at every spawn site that later
/// relies on the tree kill: a shim shell is not an MSYS process, so neither the
/// MSYS subtree sweep nor `TerminateJobObject` can reach the command's own
/// processes (`sleep 30 &` survives an abort/timeout; measured).
pub(crate) fn shell_spawn_target(shell_program: &str) -> (String, Option<String>) {
    #[cfg(windows)]
    if let Some((real, path)) = msys_shim_spawn_target(shell_program) {
        return (real, Some(path));
    }
    (shell_program.to_owned(), None)
}

/// Apply the platform spawn attributes the tree kill relies on: an own
/// process group on Unix (upstream `detached: process.platform !== "win32"`).
pub(crate) fn apply_tree_kill_spawn_attributes(spawn: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        spawn.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = spawn;
    }
}

// ---------------------------------------------------------------------------
// Streaming partial results (upstream `emitOutputUpdate`)
// ---------------------------------------------------------------------------

/// Upstream `BASH_UPDATE_THROTTLE_MS` (renderers/bash.ts): minimum spacing
/// between streamed rolling-snapshot updates.
pub(crate) const UPDATE_THROTTLE_MS: u128 = 100;

/// Build the partial-result payload for a shell streaming update (upstream
/// `emitOutputUpdate`): `{content: [{type: "text", text}], details?, isError}`.
pub(crate) fn shell_update_payload(
    text: &str,
    details: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "content".to_owned(),
        serde_json::json!([{ "type": "text", "text": text }]),
    );
    if let Some(details) = details {
        payload.insert("details".to_owned(), details);
    }
    payload.insert("isError".to_owned(), serde_json::json!(false));
    serde_json::Value::Object(payload)
}

/// Upstream `appendStatus`: ``text ? `${text}\n\n${status}` : status``.
pub(crate) fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_owned()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// Spacing of the "still running" progress note emitted when a command has
/// produced no output for this long. Upstream has no heartbeat (its
/// `emitOutputUpdate` only fires when output arrived); pir's version is a
/// deliberate difference so a silent long-running command is visibly alive in
/// the TUI / extension `tool_execution_update` observers. The model is blocked
/// on the call either way; this never changes the tool result or the command.
pub(crate) const SHELL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Build the periodic progress update: the captured output so far (windowed
/// like a normal streaming snapshot) plus an elapsed-time status line.
pub(crate) fn shell_heartbeat_payload(
    capture: &ShellCapture,
    elapsed: std::time::Duration,
) -> serde_json::Value {
    // Borrowed tail: the heartbeat fires while a silent command runs, and the
    // window it renders allocates its own retained text below.
    let view = truncate_tail(&capture.tail_lossy(), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let status = format!(
        "Command still running ({}s elapsed, no new output for {}s)",
        elapsed.as_secs(),
        SHELL_HEARTBEAT_INTERVAL.as_secs()
    );
    shell_update_payload(&append_status(&view.content, &status), None)
}

/// Spawn a pipe reader feeding every read chunk to `on_chunk` until EOF: the
/// interleaved stdout/stderr capture loop shared by the shell tools (the
/// 8KiB read buffer and the Ok(0)/Err stop conditions match upstream's
/// OutputAccumulator consumers).
pub(crate) fn spawn_pipe_reader<R, F>(mut pipe: R, on_chunk: F) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    F: FnMut(&[u8]) + Send + 'static,
{
    let mut on_chunk = on_chunk;
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => on_chunk(&buf[..n]),
            }
        }
    })
}

/// Apply the PI_* session environment for a spawned shell child (upstream
/// `resolveSpawnContext`, bash.ts:193-209): previous values are always cleared
/// first, then what the session context carries is re-exposed.
///
/// `base_path` is a `PATH` the caller already queued on the command (the MSYS
/// shim override); the `pir` binary's directory is prepended to it rather than
/// replacing it.
pub(crate) fn apply_session_env(
    spawn: &mut tokio::process::Command,
    session_env: Option<&crate::agent::tool_execution::ToolSessionEnv>,
    base_path: Option<&str>,
) {
    for name in [
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
    ] {
        spawn.env_remove(name);
    }
    if let Some(env) = session_env {
        if let Some(session_id) = env.session_id.as_deref().filter(|id| !id.is_empty()) {
            spawn.env("PI_SESSION_ID", session_id);
        }
        if let Some(session_file) = &env.session_file {
            spawn.env("PI_SESSION_FILE", session_file);
        }
        spawn.env("PI_PROVIDER", &env.model.provider);
        spawn.env("PI_MODEL", &env.model.id);
        spawn.env("PI_REASONING_LEVEL", env.thinking.wire_key());
    }
    apply_pir_bin_env(spawn, base_path);
}

/// The PI_* session variables as an environment-override map for backends
/// that receive an explicit environment instead of inheriting the process one
/// (the remote bash arm and pluggable [`BashOperations`]). `None` removes a
/// variable, mirroring upstream's delete-then-set order so an inherited value
/// can never leak into the child.
pub(crate) fn session_env_overrides(
    session_env: Option<&crate::agent::tool_execution::ToolSessionEnv>,
) -> std::collections::BTreeMap<String, Option<String>> {
    let mut env = std::collections::BTreeMap::new();
    env.insert("PI_SESSION_ID".to_string(), None);
    env.insert("PI_SESSION_FILE".to_string(), None);
    env.insert("PI_PROVIDER".to_string(), None);
    env.insert("PI_MODEL".to_string(), None);
    env.insert("PI_REASONING_LEVEL".to_string(), None);
    if let Some(session) = session_env {
        if let Some(session_id) = session.session_id.as_deref().filter(|id| !id.is_empty()) {
            env.insert("PI_SESSION_ID".to_string(), Some(session_id.to_owned()));
        }
        if let Some(session_file) = &session.session_file {
            env.insert(
                "PI_SESSION_FILE".to_string(),
                Some(session_file.to_string_lossy().into_owned()),
            );
        }
        env.insert(
            "PI_PROVIDER".to_string(),
            Some(session.model.provider.to_string()),
        );
        env.insert("PI_MODEL".to_string(), Some(session.model.id.clone()));
        env.insert(
            "PI_REASONING_LEVEL".to_string(),
            Some(session.thinking.wire_key().to_string()),
        );
    }
    env
}

/// Expose the running `pir` binary to the child shell: `PIR_BIN` holds its
/// absolute path and the binary's directory is prepended to the child `PATH`,
/// so a command can call `pir grep|sed|python …` directly.
pub(crate) fn apply_pir_bin_env(spawn: &mut tokio::process::Command, base_path: Option<&str>) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    spawn.env("PIR_BIN", &executable);
    let Some(directory) = executable.parent() else {
        return;
    };
    let existing = base_path
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"));
    let Some(existing) = existing else {
        spawn.env("PATH", directory);
        return;
    };
    let mut entries = vec![directory.to_path_buf()];
    entries.extend(std::env::split_paths(&existing));
    match std::env::join_paths(entries) {
        Ok(joined) => {
            spawn.env("PATH", joined);
        }
        Err(_) => {
            // A path component contains the platform separator: fall back to
            // plain concatenation rather than dropping the prepend.
            let separator = if cfg!(windows) { ';' } else { ':' };
            let mut joined = directory.as_os_str().to_os_string();
            joined.push(separator.to_string());
            joined.push(existing);
            spawn.env("PATH", joined);
        }
    }
}

/// Upstream `bashOutputSchema`: the structured result for programmatic
/// callers, shared by the bash and powershell tools (upstream builds the
/// PowerShell tool's schema from the same definition). A non-zero exit code is
/// an error result for the model, but scripts still resolve to this value.
pub(crate) fn shell_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "output": { "type": "string", "description": "Combined stdout and stderr, possibly truncated" },
            "truncated": { "type": "boolean" },
            "full_output_path": { "type": "string", "description": "Full output, when truncated" },
            "exit_code": { "type": "number" },
            "wall_time_seconds": { "type": "number" }
        },
        "required": ["output", "truncated", "exit_code", "wall_time_seconds"]
    })
}

#[cfg(feature = "tool-bash")]
impl AgentTool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds; when omitted, the tools.commandTimeoutMs setting applies (120 seconds by default). When PIR_BIN is set, prefer `\"$PIR_BIN\" grep|sed|python …` over the system grep/sed/python; use the plain commands only when PIR_BIN is unavailable or pir rejects the usage."
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Execute bash commands; prefer pir grep|sed|python ($PIR_BIN) when available"
                .to_owned(),
        )
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec![
            "You can inspect PI_* environment variables for current model and session details."
                .to_owned(),
        ]
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    fn has_external_side_effects(&self) -> bool {
        true
    }
    /// Async pool warm-up: when bash is in the callable set and the pool is
    /// enabled, start the persistent member in the background so the first
    /// real call skips the ~300 ms process-creation cost. Idempotent with
    /// the lazy get-or-start path; failures are ignored.
    fn prepare_loadout(
        &self,
        loadout: &crate::agent::tool_execution::ToolLoadout,
    ) -> Option<crate::agent::tool_execution::ToolLoadoutChanges> {
        if loadout.callable.iter().any(|tool| tool.name() == "bash") {
            super::shell_pool::prewarm(super::shell_pool::ShellProtocol::Bash);
        }
        None
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional; when omitted, the tools.commandTimeoutMs setting applies, 120 seconds by default)" }
            },
            "required": ["command"]
        }))
    }
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(shell_output_schema())
    }
    /// Upstream `bash.ts:260`: the bash tool prefers strict JSON-schema
    /// constrained sampling.
    fn constrained_sampling(&self) -> Option<crate::ai::types::ConstrainedSamplingConfig> {
        Some(crate::ai::types::ConstrainedSamplingConfig::JsonSchema {
            strict: crate::ai::types::StrictPreference::Prefer,
        })
    }
    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Upstream bash.ts: timeout is OPTIONAL (no default), may be
        // fractional, and is capped at 2_147_483 seconds; a non-finite or
        // non-positive value is rejected.
        let call_timeout_secs =
            match validate_timeout_secs(args.get("timeout").and_then(|v| v.as_f64())) {
                Ok(timeout) => timeout,
                Err(error) => return Box::pin(async move { ToolResult::error(error) }),
            };
        let cwd = ctx.cwd.clone();
        let signal = ctx.signal.clone();
        // Session metadata for the PI_* child environment (upstream
        // `resolveSpawnContext`). pir's ToolContext carries the session model
        // and thinking level but no session id/file, so those two variables
        // are only ever cleared here.
        let session_env = ctx.session_env.clone();
        // Shell preferences (settings `shellCommandPrefix` / `shellPath` /
        // `shellPool` / `tools.commandTimeoutMs`).
        let prefs = crate::config::settings::current_shell_prefs();
        // Effective timeout: the per-call argument wins; otherwise
        // `tools.commandTimeoutMs` applies (120 s unless configured or
        // disabled with 0 — a deliberate deviation from upstream). A detached
        // background call skips the settings default: it was explicitly
        // started to outlive the turn, so only an explicit timeout caps it.
        let setting_timeout = if ctx.background {
            None
        } else {
            prefs.command_timeout()
        };
        let (timeout_secs, timeout_from_setting) =
            resolve_command_timeout(call_timeout_secs, setting_timeout);
        // The settings `shellCommandPrefix` applies to the command text in both
        // arms (it is shell syntax, executed where the command runs).
        let command = apply_command_prefix(prefs.command_prefix.as_deref(), &command);
        // An explicitly injected backend wins over the process-wide env_client
        // selection (upstream `BashToolOptions.operations`).
        if let Some(operations) = self.operations.clone() {
            let on_update = ctx.update_channel();
            return Box::pin(async move {
                if signal.as_ref().is_some_and(|s| s.is_aborted()) {
                    return ToolResult::error("Command aborted");
                }
                run_with_operations(
                    operations,
                    &command,
                    &cwd,
                    timeout_secs,
                    timeout_from_setting,
                    session_env,
                    on_update,
                    signal,
                )
                .await
            });
        }
        // The env indirection layer (`env_client::selection`): checked before
        // the local shell is resolved, so a machine without a local bash can
        // still drive a remote environment. `None` — the default, no override
        // and no `PI_ENV_SSH_HOST` — leaves the local path below byte-identical.
        if let Some(environment) = crate::env_client::configured_remote_env() {
            let on_update = ctx.update_channel();
            return Box::pin(async move {
                if signal.as_ref().is_some_and(|s| s.is_aborted()) {
                    return ToolResult::error("Command aborted");
                }
                run_remote_bash(
                    environment,
                    &command,
                    timeout_secs,
                    timeout_from_setting,
                    session_env.as_deref(),
                    on_update,
                    signal.clone(),
                )
                .await
            });
        }
        let shell = match resolve_shell_config(prefs.shell_path.as_deref()) {
            Ok(shell) => shell,
            Err(error) => return Box::pin(async move { ToolResult::error(error) }),
        };
        // Persistent-shell pool switch (settings `shellPool`, default on).
        let pool_enabled = prefs.pool;
        // Live partial-result channel (upstream tool `onUpdate`).
        let on_update = ctx.update_channel();
        let background = ctx.background;
        Box::pin(async move {
            if signal.as_ref().is_some_and(|s| s.is_aborted()) {
                return ToolResult::error("Command aborted");
            }
            if !cwd.is_dir() {
                return ToolResult::error(format!(
                    "Working directory '{}' does not exist",
                    cwd.display()
                ));
            }

            let started_at = std::time::Instant::now();
            // Pooled fast path: a long-lived persistent bash member runs every
            // command inside an isolating subshell with a sentinel protocol
            // (shell_pool.rs maps the upstream fresh-shell semantics). The
            // fallbacks — pool off, legacy-WSL stdin transport, member spawn
            // or handshake failure, and a script-write failure before the
            // command is dispatched — take the per-call spawn path below.
            // Protocol loss after the write is terminal (captured output, no
            // replay).
            let mut initial_update_emitted = false;
            if pool_enabled && !shell.stdin_transport() && !background {
                if let Some(outcome) = super::shell_pool::global()
                    .execute(
                        super::shell_pool::ShellProtocol::Bash,
                        shell.program(),
                        &cwd,
                        &command,
                        session_env.as_deref(),
                        2 * DEFAULT_MAX_BYTES,
                        timeout_secs,
                        signal.as_ref(),
                        on_update.as_ref(),
                    )
                    .await
                {
                    if let Some(result) =
                        pooled_tool_result(outcome, started_at, timeout_secs, timeout_from_setting)
                    {
                        return result;
                    }
                    // The script write failed before the command was
                    // dispatched: fall back WITHOUT re-emitting the initial
                    // update.
                    initial_update_emitted = true;
                }
                // None: pool not usable this call — the per-call path
                // below emits its own initial update.
            }
            // The abort may have fired while the pool attempt waited for its
            // member (up to the 5 s dispatch window) or fell back: a command
            // must never start after the abort.
            if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                return ToolResult::error("Command aborted");
            }
            // Spawn target: the resolved shell, or the real MSYS bash when the
            // resolved one is a Git-for-Windows launcher shim
            // ([`shell_spawn_target`]). A shim shell is not an MSYS process, so
            // the abort/timeout tree kill below (which runs the MSYS subtree
            // sweep and the job) could not reach the command's own processes —
            // an aborted per-call command kept its forked descendants running
            // (measured, same shape as the pool member's).
            let (spawn_program, spawn_path) = shell_spawn_target(shell.program());
            let mut spawn = tokio::process::Command::new(&spawn_program);
            if let Some(path) = spawn_path.as_deref() {
                spawn.env("PATH", path);
            }
            spawn
                .args(shell.args())
                .current_dir(&cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                // CREATE_NO_WINDOW (0x08000000): the per-call shell must not
                // flash a console window (upstream `windowsHide: true`).
                spawn.creation_flags(0x0800_0000);
            }
            apply_tree_kill_spawn_attributes(&mut spawn);
            // PI_* session environment (upstream `resolveSpawnContext`).
            apply_session_env(&mut spawn, session_env.as_deref(), spawn_path.as_deref());
            if shell.stdin_transport() {
                // Legacy WSL bash: the command arrives on stdin.
                spawn.stdin(std::process::Stdio::piped());
            } else {
                spawn.arg(&command);
                // Upstream `stdio: [commandFromStdin ? "pipe" : "ignore", ...]`
                // (bash.ts:116): never inherit the host's stdin — a host whose
                // stdin is a held-open pipe (RPC mode) makes Git Bash block at
                // startup until that pipe closes.
                spawn.stdin(std::process::Stdio::null());
            }
            let mut child = match spawn.spawn() {
                Ok(child) => child,
                Err(e) => return ToolResult::error(format!("Command failed: {e}")),
            };
            // Tree-kill handle: a Job Object on Windows (fallback taskkill
            // /T /F), the child's own process group on Unix (upstream
            // `killProcessTree` fires on abort/timeout). The RAII guard
            // covers the paths that leave this future without reaching the
            // explicit kill below (caller cancellation, panic, an early
            // return).
            let mut tree_kill = TreeKillGuard::arm(&child);
            if shell.stdin_transport() {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(command.as_bytes()).await;
                    drop(stdin);
                }
            }

            // Interleaved stdout/stderr capture into a shared bounded buffer.
            let capture = std::sync::Arc::new(std::sync::Mutex::new(ShellCapture::new(
                2 * DEFAULT_MAX_BYTES,
            )));
            let Some(stdout) = child.stdout.take() else {
                return ToolResult::error("Command failed: stdout pipe unavailable");
            };
            let Some(stderr) = child.stderr.take() else {
                return ToolResult::error("Command failed: stderr pipe unavailable");
            };
            let cap_out = capture.clone();
            let cap_err = capture.clone();
            let out_task = spawn_pipe_reader(stdout, move |chunk| {
                if let Ok(mut guard) = cap_out.lock() {
                    guard.append(chunk);
                }
            });
            let err_task = spawn_pipe_reader(stderr, move |chunk| {
                if let Ok(mut guard) = cap_err.lock() {
                    guard.append(chunk);
                }
            });

            // Wait for exit / timeout / abort on their own futures: the child
            // wait wakes the moment the process exits instead of a 50 ms poll
            // tick, and the rolling snapshot rides a throttled interval branch.
            let deadline = timeout_secs
                .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
            let mut timed_out = false;
            let mut aborted = false;
            // Live partial-result streaming (upstream `emitOutputUpdate` with
            // BASH_UPDATE_THROTTLE_MS): the initial empty snapshot attaches
            // the running tool, then throttled rolling snapshots follow. The
            // pool-protocol-loss fallback already emitted it.
            let mut last_update_total = 0usize;
            // Heartbeat clock: reset whenever an output snapshot is emitted,
            // so a note only appears after a full silent interval.
            let mut last_heartbeat_at = std::time::Instant::now();
            if !initial_update_emitted {
                if let Some(emit) = &on_update {
                    let mut initial = serde_json::Map::new();
                    initial.insert("content".to_owned(), serde_json::json!([]));
                    emit(serde_json::Value::Object(initial));
                }
            }
            let update_period = std::time::Duration::from_millis(UPDATE_THROTTLE_MS as u64);
            let mut update_ticker = tokio::time::interval_at(
                tokio::time::Instant::now() + update_period,
                update_period,
            );
            update_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let deadline_wait = async {
                match deadline {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline_wait);
            let abort_wait = async {
                match signal.as_ref() {
                    Some(signal) => signal.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(abort_wait);
            // `child.wait()` borrows the child mutably, so it is scoped to this
            // block; the timeout/abort kill runs once the borrow is dropped.
            let wait_status = {
                let wait = child.wait();
                tokio::pin!(wait);
                let mut status = None;
                loop {
                    tokio::select! {
                        biased;
                        result = &mut wait => {
                            status = Some(result);
                            break;
                        }
                        _ = &mut abort_wait => {
                            aborted = true;
                            break;
                        }
                        _ = &mut deadline_wait => {
                            timed_out = true;
                            break;
                        }
                        _ = update_ticker.tick() => {
                            // Stream a throttled rolling snapshot when new
                            // output arrived; if nothing arrived for a full
                            // heartbeat interval, emit a "still running" note
                            // carrying the output so far.
                            if let Some(emit) = &on_update {
                                let snapshot = capture
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .update_snapshot(&mut last_update_total);
                                match snapshot {
                                    Some(payload) => {
                                        emit(payload);
                                        last_heartbeat_at = std::time::Instant::now();
                                    }
                                    None if last_heartbeat_at.elapsed()
                                        >= SHELL_HEARTBEAT_INTERVAL =>
                                    {
                                        let guard = capture
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                                        emit(shell_heartbeat_payload(
                                            &guard,
                                            started_at.elapsed(),
                                        ));
                                        drop(guard);
                                        last_heartbeat_at = std::time::Instant::now();
                                    }
                                    None => {}
                                }
                            }
                        }
                    }
                }
                status
            };
            if wait_status.is_none() {
                // Upstream `killProcessTree`: the whole descendant tree dies,
                // not just the shell. The sweep is dispatched first (the MSYS
                // walk needs the root's table entry) but does not block this
                // async path.
                tree_kill.terminate();
                let _ = child.start_kill();
            }
            // Upstream `waitForChildProcess` (utils/child-process.ts:88-97):
            // after the child exited (or the timeout/abort kill fired), the
            // pipe readers must NOT be joined unconditionally — a detached
            // grandchild that inherited the stdout/stderr write ends holds
            // them open forever, and the join would block past the timeout
            // (which is no longer being polled here). Wait a bounded quiet
            // window, re-armed on every captured chunk so a still-writing
            // descendant is drained; when it expires, abort the readers
            // (cancelling at the pending `read` drops the pipes, closing the
            // read ends) and proceed with whatever was captured. The absolute
            // cap above bounds the re-arming window: a descendant that keeps
            // writing cannot extend the drain past it.
            let exit_at = std::time::Instant::now();
            loop {
                if out_task.is_finished() && err_task.is_finished() {
                    break;
                }
                let last_data = capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_chunk_at();
                let now = std::time::Instant::now();
                if now.duration_since(exit_at) >= EXIT_STDIO_ABSOLUTE_CAP
                    || now.duration_since(exit_at.max(last_data)) >= EXIT_STDIO_GRACE
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            out_task.abort();
            err_task.abort();
            let _ = tokio::join!(out_task, err_task);
            // Final snapshot flush (upstream `finishOutput`'s emit).
            if let Some(emit) = &on_update {
                let payload = capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .update_snapshot(&mut last_update_total);
                if let Some(payload) = payload {
                    emit(payload);
                }
            }
            // The wait resolved in the select above when the child exited on
            // its own; a timeout/abort kill still needs its reaping wait.
            let status = match wait_status {
                Some(status) => status,
                None => child.wait().await,
            };
            // Past this point the run is over on its own terms: intentionally
            // backgrounded descendants keep running (upstream kills only on
            // abort/timeout), so the RAII guard must not fire at scope exit.
            tree_kill.disarm();

            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Exit code for the shared result builder: the child's exit
            // status; abort/timeout carry none, and a signal-killed shell
            // maps to the shell convention 128+signal (upstream).
            let exit_status = status.as_ref().ok();
            let mut exit_code = exit_status.and_then(|s| s.code()).map(i64::from);
            if exit_code.is_none() && !aborted && !timed_out {
                exit_code = map_signal_exit_code(exit_status);
            }
            finalize_shell_result(
                &mut guard,
                started_at,
                exit_code,
                aborted,
                timed_out,
                timeout_secs,
                timeout_from_setting,
            )
        })
    }
}

/// The remote arm of the bash tool (the env indirection layer's `Some`
/// branch): the command runs through `RemoteExecutionEnv::exec` on the other
/// machine, and its streamed output feeds the *same* [`ShellCapture`] and
/// [`finalize_shell_result`] the local arm uses, so the model-visible result
/// (rolling tail, truncation footer, status line, structured content) is
/// identical for identical output.
///
/// Deltas from the local arm, all consequences of the command running
/// elsewhere:
/// * the working directory is the environment's own (`RemoteExecutionEnv`
///   root, i.e. `hello.cwd` or `PI_ENV_REMOTE_CWD`), and "does not exist" is
///   checked through the daemon's `lstat`;
/// * the shell is the **remote** machine's: the settings `shellPath` names
///   this machine's shell and is local-arm only (upstream feeds it only to
///   `createLocalBashOperations`), so the daemon resolves its own unless
///   `PI_ENV_REMOTE_SHELL_PATH` configures one (the construction-option
///   analogue of upstream `RemoteExecutionEnvOptions.shellPath`, which
///   upstream leaves to its caller — `env_client::selection` is that caller);
/// * timeout and abort are the daemon's (`timeout` / `aborted` error frames),
///   not a local `ProcessTreeKill`;
/// * the full-output file of a truncated run stays on **this** machine, as
///   upstream coding-agent's does: its `executeBashWithOperations` wraps *any*
///   `BashOperations`, remote ones included, in a client-side accumulator and
///   creates the temp file under this process's `tmpdir()`
///   (`core/bash-executor.ts:11,66` for extensions,
///   `core/tools/output-accumulator.ts:249` for the tool). The daemon's
///   `spill` option and `spillPath` reply belong to `packages/durable`'s bash
///   tool, which pir does not port. Upstream's consequence is kept: a
///   truncated run's footer points at a path *this* machine can read, which
///   the remote `read` tool cannot fetch.
async fn run_remote_bash(
    environment: std::sync::Arc<crate::env_client::RemoteExecutionEnv>,
    command: &str,
    timeout_secs: Option<f64>,
    timeout_from_setting: bool,
    session_env: Option<&crate::agent::tool_execution::ToolSessionEnv>,
    on_update: Option<crate::agent::tool_execution::ToolUpdateFn>,
    signal: Option<crate::ai::types::AbortSignal>,
) -> ToolResult {
    let started_at = std::time::Instant::now();
    let cwd = match environment.root().await {
        Ok(cwd) => cwd,
        Err(error) => return ToolResult::error(format!("Command failed: {}", error.message)),
    };
    // The abort signal rides the env client's Context: the connection sends
    // its cancel frame on `signal.cancelled()` and the daemon answers with an
    // `aborted` error frame (previously dropped here, so Esc/Ctrl+C could not
    // stop a remote command).
    let context = crate::env_client::Context::new(signal.clone());
    // The local arm's `cwd.is_dir()` check, through the daemon.
    let is_directory = matches!(
        environment.file_info(&cwd, &context).await,
        Ok(info) if info.kind == crate::env_client::FileKind::Directory
    );
    if !is_directory {
        return ToolResult::error(format!("Working directory '{cwd}' does not exist"));
    }
    // PI_* session environment (upstream `resolveSpawnContext`): every
    // variable is cleared first, then the session's values are set.
    let env = session_env_overrides(session_env);
    let capture = std::sync::Arc::new(std::sync::Mutex::new(ShellCapture::new(
        2 * DEFAULT_MAX_BYTES,
    )));
    // Live partial-result streaming (upstream `emitOutputUpdate` with
    // BASH_UPDATE_THROTTLE_MS): the initial empty snapshot attaches the
    // running tool, then throttled rolling snapshots follow from the output
    // callback.
    if let Some(emit) = &on_update {
        let mut initial = serde_json::Map::new();
        initial.insert("content".to_owned(), serde_json::json!([]));
        emit(serde_json::Value::Object(initial));
    }
    let capture_for_output = capture.clone();
    let update_sink = on_update.clone();
    let update_total = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let update_total_for_output = update_total.clone();
    let update_at = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let update_at_for_output = update_at.clone();
    let on_output: crate::env_client::OutputFn = std::sync::Arc::new(move |text: &str, _info| {
        let mut guard = capture_for_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.append(text.as_bytes());
        let Some(emit) = &update_sink else {
            return;
        };
        let due = update_at_for_output
            .lock()
            .map(|at| at.elapsed().as_millis() >= UPDATE_THROTTLE_MS)
            .unwrap_or(false);
        if !due {
            return;
        }
        let mut total = update_total_for_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(payload) = guard.update_snapshot(&mut total) {
            emit(payload);
        }
        drop(total);
        if let Ok(mut at) = update_at_for_output.lock() {
            *at = std::time::Instant::now();
        }
    });
    let options = crate::env_client::ExecOptions {
        cwd: Some(cwd),
        env,
        inherit_env: Some(true),
        timeout_secs,
        // Upstream coding-agent never asks its environment to spill, remote or
        // not (see the `run_remote_bash` delta note): the local `ShellCapture`
        // owns the full-output file, like upstream's client-side
        // `OutputAccumulator`.
        spill: None,
        window: None,
        on_output: Some(on_output),
    };
    let exec_command = crate::env_client::ExecCommand::Shell(command.to_string());
    let exec = environment.exec(&exec_command, &options, &context);
    tokio::pin!(exec);
    // Silent-command heartbeat (the local arms' `SHELL_HEARTBEAT_INTERVAL`
    // note): the daemon enforces the timeout; this only keeps a silent remote
    // command visibly alive. The capture's last-chunk instant resets it.
    let mut heartbeat_ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + SHELL_HEARTBEAT_INTERVAL,
        SHELL_HEARTBEAT_INTERVAL,
    );
    heartbeat_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            result = &mut exec => break result,
            _ = heartbeat_ticker.tick() => {
                if let Some(emit) = &on_update {
                    let guard = capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.last_chunk_at().elapsed() >= SHELL_HEARTBEAT_INTERVAL {
                        emit(shell_heartbeat_payload(&guard, started_at.elapsed()));
                    }
                }
            }
        }
    };
    // Final snapshot flush (upstream `finishOutput`'s emit).
    let mut guard = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(emit) = &on_update {
        let mut total = update_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(payload) = guard.update_snapshot(&mut total) {
            emit(payload);
        }
    }
    match result {
        Ok(executed) => finalize_shell_result(
            &mut guard,
            started_at,
            Some(executed.exit_code),
            false,
            false,
            timeout_secs,
            timeout_from_setting,
        ),
        Err(error) => match error.code {
            crate::env_client::ExecutionErrorCode::Aborted => finalize_shell_result(
                &mut guard,
                started_at,
                None,
                true,
                false,
                timeout_secs,
                timeout_from_setting,
            ),
            crate::env_client::ExecutionErrorCode::Timeout => finalize_shell_result(
                &mut guard,
                started_at,
                None,
                false,
                true,
                timeout_secs,
                timeout_from_setting,
            ),
            _ => ToolResult::error(format!("Command failed: {}", error.message)),
        },
    }
}

/// The pluggable-backend arm of the bash tool: the command runs through an
/// injected [`BashOperations`] while this side owns the rolling capture,
/// streaming updates, truncation footer and structured content — the same
/// shaping as the local and remote arms, so a backend only has to stream
/// bytes and report an exit code.
// The per-call knobs are a fixed set threaded from the tool arm; a parameter
// struct would only repeat them at the single call site.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with_operations(
    operations: std::sync::Arc<dyn BashOperations>,
    command: &str,
    cwd: &std::path::Path,
    timeout_secs: Option<f64>,
    timeout_from_setting: bool,
    session_env: Option<std::sync::Arc<crate::agent::tool_execution::ToolSessionEnv>>,
    on_update: Option<crate::agent::tool_execution::ToolUpdateFn>,
    signal: Option<crate::ai::types::AbortSignal>,
) -> ToolResult {
    let started_at = std::time::Instant::now();
    let capture = std::sync::Arc::new(std::sync::Mutex::new(ShellCapture::new(
        2 * DEFAULT_MAX_BYTES,
    )));
    if let Some(emit) = &on_update {
        let mut initial = serde_json::Map::new();
        initial.insert("content".to_owned(), serde_json::json!([]));
        emit(serde_json::Value::Object(initial));
    }
    let update_total = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let update_at = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let capture_for_output = capture.clone();
    let update_sink = on_update.clone();
    let update_total_for_output = update_total.clone();
    let update_at_for_output = update_at.clone();
    let on_data: BashDataCallback = std::sync::Arc::new(move |bytes: &[u8]| {
        let mut guard = capture_for_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.append(bytes);
        let Some(emit) = &update_sink else {
            return;
        };
        let due = update_at_for_output
            .lock()
            .map(|at| at.elapsed().as_millis() >= UPDATE_THROTTLE_MS)
            .unwrap_or(false);
        if !due {
            return;
        }
        let mut total = update_total_for_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(payload) = guard.update_snapshot(&mut total) {
            emit(payload);
        }
        drop(total);
        if let Ok(mut at) = update_at_for_output.lock() {
            *at = std::time::Instant::now();
        }
    });
    let options = BashExecOptions {
        on_data,
        signal: signal.clone(),
        timeout: timeout_secs,
        env: session_env_overrides(session_env.as_deref()),
    };
    let exec = operations.exec(command, cwd, options);
    tokio::pin!(exec);
    let mut heartbeat_ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + SHELL_HEARTBEAT_INTERVAL,
        SHELL_HEARTBEAT_INTERVAL,
    );
    heartbeat_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            result = &mut exec => break result,
            _ = heartbeat_ticker.tick() => {
                if let Some(emit) = &on_update {
                    let guard = capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.last_chunk_at().elapsed() >= SHELL_HEARTBEAT_INTERVAL {
                        emit(shell_heartbeat_payload(&guard, started_at.elapsed()));
                    }
                }
            }
        }
    };
    let mut guard = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(emit) = &on_update {
        let mut total = update_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(payload) = guard.update_snapshot(&mut total) {
            emit(payload);
        }
    }
    match result {
        Ok(exit_code) => finalize_shell_result(
            &mut guard,
            started_at,
            exit_code,
            false,
            false,
            timeout_secs,
            timeout_from_setting,
        ),
        Err(message) if message == "aborted" => finalize_shell_result(
            &mut guard,
            started_at,
            None,
            true,
            false,
            timeout_secs,
            timeout_from_setting,
        ),
        Err(message) if message.starts_with("timeout:") => {
            let reported = message
                .split_once(':')
                .and_then(|(_, value)| value.trim().parse::<f64>().ok())
                .or(timeout_secs);
            finalize_shell_result(
                &mut guard,
                started_at,
                None,
                false,
                true,
                reported,
                timeout_from_setting,
            )
        }
        Err(message) => ToolResult::error(message),
    }
}

/// A signal-killed shell has no exit code: upstream reports the standard
/// shell convention 128+signal (bash.ts `signalCode`) so callers do not
/// mistake the termination for a successful command.
#[cfg(unix)]
fn map_signal_exit_code(status: Option<&std::process::ExitStatus>) -> Option<i64> {
    use std::os::unix::process::ExitStatusExt;
    status
        .and_then(|s| s.signal())
        .map(|sig| 128 + i64::from(sig))
}

/// Non-Unix: no signal mapping (upstream reports no exit code).
#[cfg(not(unix))]
fn map_signal_exit_code(_status: Option<&std::process::ExitStatus>) -> Option<i64> {
    None
}

/// Build the final [`ToolResult`] for a finished shell run — shared by the
/// per-call spawn path and the pooled path so both produce byte-identical
/// `content` / `details` / `structuredContent` (upstream bash.ts
/// `formatOutput` + the structured-content shaping).
fn finalize_shell_result(
    capture: &mut ShellCapture,
    started_at: std::time::Instant,
    exit_code: Option<i64>,
    aborted: bool,
    timed_out: bool,
    timeout_secs: Option<f64>,
    timeout_from_setting: bool,
) -> ToolResult {
    let content = capture.tail_string();
    // 状态条附在输出之后（上游 appendStatus 语义）。
    let status_line = if aborted {
        Some("Command aborted".to_owned())
    } else if timed_out {
        Some(format_command_timed_out(timeout_secs, timeout_from_setting))
    } else {
        match exit_code {
            None => Some("Command terminated without an exit code".to_owned()),
            Some(0) => None,
            Some(code) => Some(format!("Command exited with code {code}")),
        }
    };
    // Raw captured output only; the status line is appended AFTER
    // truncation in the result branches below (upstream
    // `appendStatus` runs post-`truncateTail`). Folding it into
    // `text` here as well appended it twice.
    let text = content;

    // Structured content for programmatic callers (upstream
    // bashOutputSchema): full output up to 1MB, exit code, wall time.
    // Aborted/timed-out runs throw upstream and carry no structured
    // content, so only the completed paths set it.
    //
    // SIZE SHAPE of one large truncated result (measured on a real 5 000-row
    // run: `scripts/tui_conpty/out/w16k-final/s49_long_output_spill_expand`).
    // The same body travels in three fields of the result:
    //   * `content` — retained tail + `[Showing lines …]` footer (51 337 B).
    //     The model-facing text and every reader's body: provider requests,
    //     the TUI (collapsed/expanded/ctrl+o), replay, `/export`, compaction.
    //   * `details.truncation.content` — the same tail without the footer
    //     (51 183 B), from `TruncationResult::to_details_json`. Upstream
    //     `BashToolDetails` persists the whole `TruncationResult`, so this is
    //     session-format parity; readers use only its counters.
    //   * `structuredContent.output` — the FULL stream (140 000 B here, capped
    //     at `STRUCTURED_OUTPUT_MAX_BYTES`). It is an execution-time channel:
    //     codemode scripts and `tool_result` handlers read the live
    //     `ToolResult`, never a session record. Upstream's persisted
    //     `ToolResultMessage` has no `structuredContent` field at all, so the
    //     session writer drops this copy
    //     (`session_manager::types::session_tool_result`). Do not remove it
    //     from the result itself — programmatic callers need it.
    let structured_content = if !aborted && !timed_out {
        match exit_code {
            Some(code) => {
                let (full_output, truncated) = capture.full_output(STRUCTURED_OUTPUT_MAX_BYTES);
                let wall_time_seconds =
                    (started_at.elapsed().as_millis() as f64 / 100.0).round() / 10.0;
                let mut structured = serde_json::json!({
                    "output": full_output,
                    "truncated": truncated,
                    "exit_code": serde_json::json!(code),
                    "wall_time_seconds": wall_time_seconds,
                });
                if truncated {
                    if let Some(path) = capture.spill_path() {
                        structured["full_output_path"] = serde_json::json!(path);
                    }
                }
                Some(structured)
            }
            None => None,
        }
    } else {
        None
    };
    let apply_structured = |result: &mut crate::agent::tool_batch::ToolResult| {
        if let Some(structured) = structured_content {
            result.structured_content = Some(structured);
        }
    };

    // Upstream `formatOutput`: the empty-output placeholder is
    // "(no output)" on completed runs (exit-code errors included)
    // and empty on abort/timeout (upstream passes "" in the catch
    // branch).
    let empty_text = if aborted || timed_out {
        ""
    } else {
        "(no output)"
    };

    // 截断（tail）+ 全量落盘 footer + details；totalLines/totalBytes 用增量
    // 维护的全量计数覆盖窗口统计（上游 OutputAccumulator.snapshot）。
    let truncation =
        capture.with_full_totals(truncate_tail(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES));
    if truncation.truncated {
        // The advertised full-output file must be the complete stream
        // (upstream temp-file semantics), not just the dropped prefix.
        let full_path = capture.spill_if_needed(&text);
        // Upstream `BashToolDetails` = {truncation, fullOutputPath}.
        let mut details = serde_json::Map::new();
        details.insert("truncation".to_owned(), truncation.to_details_json());
        if let Some(path) = &full_path {
            details.insert("fullOutputPath".to_owned(), serde_json::json!(path));
        }
        let rendered = format_shell_output(&truncation, full_path.as_deref(), empty_text);
        return match status_line {
            None => {
                let mut result = ToolResult::ok(rendered);
                result.details = Some(serde_json::Value::Object(details));
                apply_structured(&mut result);
                result
            }
            Some(line) => {
                let mut result = ToolResult::error(append_status(&rendered, &line));
                result.details = Some(serde_json::Value::Object(details));
                apply_structured(&mut result);
                result
            }
        };
    }
    match status_line {
        None => {
            let mut result = ToolResult::ok(if text.is_empty() {
                empty_text.to_owned()
            } else {
                text
            });
            apply_structured(&mut result);
            result
        }
        Some(line) => {
            let trimmed = text.trim_end();
            let mut result = ToolResult::error(append_status(
                if trimmed.is_empty() {
                    empty_text
                } else {
                    trimmed
                },
                &line,
            ));
            apply_structured(&mut result);
            result
        }
    }
}

/// Map a pooled [`super::shell_pool::ProtocolOutcome`] onto the shared
/// result builder. `None` means the pool could not start the command (the
/// caller falls back to the per-call spawn path); a post-write protocol loss
/// is a terminal error result carrying the captured output, never a replay.
fn pooled_tool_result(
    outcome: super::shell_pool::ProtocolOutcome,
    started_at: std::time::Instant,
    timeout_secs: Option<f64>,
    timeout_from_setting: bool,
) -> Option<ToolResult> {
    match outcome {
        super::shell_pool::ProtocolOutcome::Completed { rc, capture } => {
            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Some(finalize_shell_result(
                &mut guard,
                started_at,
                Some(rc),
                false,
                false,
                timeout_secs,
                timeout_from_setting,
            ))
        }
        super::shell_pool::ProtocolOutcome::TimedOut { capture } => {
            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Some(finalize_shell_result(
                &mut guard,
                started_at,
                None,
                false,
                true,
                timeout_secs,
                timeout_from_setting,
            ))
        }
        super::shell_pool::ProtocolOutcome::Aborted { capture } => {
            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Some(finalize_shell_result(
                &mut guard,
                started_at,
                None,
                true,
                false,
                timeout_secs,
                timeout_from_setting,
            ))
        }
        // The script was written before the protocol was lost: the command
        // may have run, so report the captured output as a terminated run
        // instead of spawning the same command again.
        super::shell_pool::ProtocolOutcome::LostAfterWrite { capture } => {
            let mut guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Some(finalize_shell_result(
                &mut guard,
                started_at,
                None,
                false,
                false,
                timeout_secs,
                timeout_from_setting,
            ))
        }
        super::shell_pool::ProtocolOutcome::Lost => None,
    }
}
