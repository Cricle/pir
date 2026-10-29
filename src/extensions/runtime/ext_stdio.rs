//! Routing for extension-originated stdio writes.
//!
//! ## The path this closes
//!
//! The JS `process` global is host-backed: `process.stdout.write` /
//! `process.stderr.write` (`api/js/process_global_stub.js.txt`) call
//! `__pir_host.writeStdout` / `__pir_host.writeStderr`, and those host
//! functions wrote the bytes to the real process stdio unconditionally
//! (`register_host_functions`). In an alt-screen TUI the real stdio IS the
//! frame, so an extension debug write (the anchor case: `pi-tool-repair`
//! under `PI_TOOL_REPAIR_DEBUG=1`) lands in the middle of the rendered
//! screen and tears it — the same corruption [`crate::logging`] was
//! introduced to route for host-side `eprintln!("[ext] …")` diagnostics.
//!
//! The `node:console` surface is already off the terminal in the extension
//! host: `console_shim.js.txt` resolves its default streams per write and
//! falls back to the prelude console's notify path unless `__pirRealStdio`
//! is set (script mode only) or the stream was replaced after boot. That
//! leaves the direct `process.stdout/stderr.write` bridge — this module.
//!
//! ## The rule (mode → destination → prefix)
//!
//! The decision reads the host's own [`ExtensionConfig`](super::ExtensionConfig)
//! (`mode`, `has_ui`), fixed at runtime construction. The interactive TUI host
//! is the only producer of the pair `mode: "interactive"` + `has_ui: true`
//! (`main_cli/run_mode.rs`, `tui/terminal.rs`; RPC is `has_ui: true` with
//! mode `"rpc"`, print is `"print"`, `pir js` is `"script"`):
//!
//! | session mode | destination | prefix |
//! |---|---|---|
//! | `interactive` / `tui` **and** `has_ui` | [`crate::logging`]: the installed `tracing` subscriber (`<agentDir>/pir.log` in the TUI; the error-tolerant stderr line when no subscriber is installed) | `[ext] stdout: ` / `[ext] stderr: ` |
//! | `rpc` / `json` (protocol hosts) | [`crate::logging`] for stdout. A protocol host owns stdout as a framed channel; a raw extension write in the middle of a frame corrupts the client's parser, so the write is routed to the diagnostic channel (stderr in RPC/JSON) instead. stderr stays byte-exact (it is already the diagnostic channel) | `[ext] stdout: ` |
//! | every other mode (`print`, `script`, `headless`, unknown) | the real process stdio, byte-exact, exactly as before this module | none |
//!
//! The headless branch is deliberately today's behavior for `pir js` scripts
//! and print mode: those stdout consumers are the user's own pipes and a
//! piping user must keep seeing the bytes (Node semantics). A protocol host
//! is different: its stdout is not user output but a JSON-line frame stream
//! (`src/coding_agent/rpc.rs` and the print/JSON wire), so letting extension
//! text through would make the stream unparseable. Only a host that owns the
//! terminal surface or a framed stdout reroutes, and both reroute to the same
//! diagnostic destination its own notices use, never into a second log.
//!
//! ## Bounding
//!
//! The routed branch is a `tracing` record, i.e. one line per write in
//! `pir.log` (or stderr in RPC/JSON); the payload is arbitrary extension text,
//! so a per-write cap ([`MAX_ROUTED_WRITE_BYTES`]) keeps one pathological write
//! from flooding the log, with the dropped byte count appended. The line is
//! then handed to [`crate::logging::ext_info`] / [`crate::logging::ext_warn`],
//! which already own repeat collapsing and the bounded tracker (so an
//! identical write in a loop is reported once per
//! [`crate::logging::REPEAT_WINDOW`], not per event). No flush/sync is forced:
//! the tracing writer appends like every other record.
//!
//! The real-stdio branch writes verbatim, uncapped — a `pir js`/`print`
//! consumer reads the exact bytes Node would have produced.

use std::io::Write;

/// Where an extension-originated stdio write goes (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtStdioRoute {
    /// The host does not own the terminal surface and stdout is not a framed
    /// protocol channel (`print`, `script`, `headless`, embedded): write the
    /// real process stdio, byte-exact and uncapped.
    RealStdio,
    /// The host owns the terminal surface (the interactive TUI): route the
    /// write through [`crate::logging`] instead of the frame.
    Routed,
    /// The host speaks a framed protocol on stdout (`rpc`, `json`): stdout is
    /// routed to [`crate::logging`] so extension text can never corrupt a
    /// protocol frame; stderr stays byte-exact.
    ProtocolSafe,
}

/// Upper bound on one routed write's payload. The routed line is appended to
/// the diagnostic log, where an unbounded payload would both flood the file
/// and (via the repeat tracker's key) hold megabytes per distinct message;
/// 4 KiB is wider than any sane debug line and still one cheap record.
pub const MAX_ROUTED_WRITE_BYTES: usize = 4096;

/// The two process streams an extension can write to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtStream {
    /// `process.stdout.write`.
    Stdout,
    /// `process.stderr.write`.
    Stderr,
}

/// Destination for one extension runtime, from its host configuration.
///
/// `mode` is matched on the raw `ExtensionConfig.mode` values (the same set
/// `normalize_mode` folds: the TUI host says `"interactive"`, `"tui"` is
/// accepted for symmetry). `has_ui` is the host's declaration that it owns
/// the terminal surface; it is required in addition to the mode, so a
/// headless harness that labels itself `"interactive"` (mode alone) keeps
/// writing its real stdio.
pub fn ext_stdio_route(mode: &str, has_ui: bool) -> ExtStdioRoute {
    match mode {
        // Framed stdout is never a user pipe, even though RPC declares a UI.
        "rpc" | "json" => ExtStdioRoute::ProtocolSafe,
        "interactive" | "tui" if has_ui => ExtStdioRoute::Routed,
        _ => ExtStdioRoute::RealStdio,
    }
}

/// Write one extension-originated chunk to the destination the route selects.
pub(crate) fn write_extension_stdio(route: ExtStdioRoute, stream: ExtStream, text: &str) {
    match route {
        ExtStdioRoute::RealStdio => write_real_stdio(stream, text),
        ExtStdioRoute::Routed => {
            let line = routed_line(stream, text);
            match stream {
                // `process.stdout` is informational, `process.stderr` is the
                // stream the extension itself called a diagnostic; both
                // survive the default `info` filter and land in the same
                // place the routed `[ext] …` diagnostics land.
                ExtStream::Stdout => crate::logging::ext_info(line),
                ExtStream::Stderr => crate::logging::ext_warn(line),
            }
        }
        ExtStdioRoute::ProtocolSafe => match stream {
            // The only stream that could corrupt the framed channel. It goes
            // to the diagnostic destination (stderr / the tracing subscriber),
            // capped like every other routed write.
            ExtStream::Stdout => crate::logging::ext_info(routed_line(stream, text)),
            // stderr is already the protocol host's diagnostic channel; a
            // direct write there is byte-exact and cannot corrupt a frame.
            ExtStream::Stderr => write_real_stdio(stream, text),
        },
    }
}

/// The routed diagnostic text: the `[ext] <stream>: ` prefix, the payload
/// with its trailing newline trimmed (the log line supplies its own), and
/// the truncation marker when the payload passed the cap.
fn routed_line(stream: ExtStream, text: &str) -> String {
    let label = match stream {
        ExtStream::Stdout => "stdout",
        ExtStream::Stderr => "stderr",
    };
    let body = text.trim_end_matches(['\r', '\n']);
    let (shown, dropped) = cap_payload(body, MAX_ROUTED_WRITE_BYTES);
    if dropped == 0 {
        format!("[ext] {label}: {shown}")
    } else {
        format!("[ext] {label}: {shown} [{dropped} more bytes truncated]")
    }
}

/// Split a payload at `max` bytes without cutting a UTF-8 character, and
/// report how many bytes the route dropped.
fn cap_payload(text: &str, max: usize) -> (&str, usize) {
    if text.len() <= max {
        return (text, 0);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}

/// The historical behavior: the chunk straight onto the real process stdio.
/// Error-tolerant — a closed pipe loses the write instead of killing the
/// host (`println!`/`eprintln!` panic on a dead handle, and a `rpc`/`print`
/// consumer may well have hung up).
fn write_real_stdio(stream: ExtStream, text: &str) {
    let bytes = text.as_bytes();
    match stream {
        ExtStream::Stdout => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(bytes);
            let _ = out.flush();
        }
        ExtStream::Stderr => {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(bytes);
            let _ = err.flush();
        }
    }
}
