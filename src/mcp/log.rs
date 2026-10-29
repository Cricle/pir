//! The MCP log (`<agentDir>/mcp.log`): server lifecycle events and stdio
//! stderr, append-only and size-bounded (upstream
//! `packages/coding-agent/src/extensions/mcp/log.ts`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value as JsonValue;

/// Default rotation cap: when the file exceeds this it is rotated to
/// `<name>.1`, keeping the on-disk footprint bounded.
const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// One line-per-entry timestamped file log, bounded by rotation.
pub struct McpLog {
    path: PathBuf,
    max_bytes: u64,
    /// The append handle plus the byte count seen by this writer. The mutex
    /// is only held to write an already-formatted line (or to rename an
    /// oversized file on the rare rotation path); the open itself happens
    /// outside it, so a burst of log lines never serializes on file-open I/O.
    inner: Mutex<LogState>,
}

/// The guarded append state. `generation` is bumped by every rotation so an
/// opener that raced the rename cannot install its now-stale handle.
struct LogState {
    file: Option<std::fs::File>,
    size: u64,
    generation: u64,
}

impl LogState {
    fn empty() -> Self {
        Self {
            file: None,
            size: 0,
            generation: 0,
        }
    }
}

impl McpLog {
    /// Open (or create) the log at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            max_bytes: DEFAULT_MAX_BYTES,
            inner: Mutex::new(LogState::empty()),
        }
    }

    /// A no-op log (used when no log path is configured).
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            max_bytes: 0,
            inner: Mutex::new(LogState::empty()),
        }
    }

    /// Whether this log writes anywhere.
    pub fn enabled(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    /// Append one line (a trailing newline is added when missing). The line
    /// and its timestamp are built before the mutex is taken; the guarded
    /// section only appends the finished bytes with one `write_all`.
    pub fn write_line(&self, line: &str) {
        if !self.enabled() {
            return;
        }
        let stamped = format!("{} {}\n", timestamp(), line.trim_end());
        loop {
            let mut open_generation: Option<u64> = None;
            {
                let mut state = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.file.is_some() && state.size > self.max_bytes {
                    // Rotate (rare): drop the handle, rename, and reopen
                    // outside the lock so the append path never opens a file
                    // while holding it.
                    self.rotate_locked(&mut state);
                } else if let Some(file) = state.file.as_mut() {
                    let _ = file.write_all(stamped.as_bytes());
                    state.size = state.size.saturating_add(stamped.len() as u64);
                    return;
                } else {
                    open_generation = Some(state.generation);
                }
            }
            let Some(generation) = open_generation else {
                continue;
            };
            let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            else {
                return;
            };
            let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.file.is_none() && state.generation == generation {
                state.file = Some(file);
                state.size = size;
            }
            // Otherwise another writer installed its handle (loop and write
            // through it) or a rotation raced this open (loop and reopen).
        }
    }

    /// Record a server lifecycle event.
    pub fn write_event(&self, server: &str, message: &str) {
        self.write_line(&format!("[{server}] {message}"));
    }

    /// Mirror one chunk of a stdio server's stderr.
    pub fn write_stderr(&self, server: &str, chunk: &str) {
        for line in chunk.lines() {
            if line.trim().is_empty() {
                continue;
            }
            self.write_line(&format!("[{server} stderr] {line}"));
        }
    }

    /// Append one `notifications/message` payload, shaped like upstream
    /// `formatMcpLogMessage` (runtime.ts:386 -> log.write): `<level> <logger>:
    /// <data>`, level defaulting to `info` and data's newlines indented.
    pub fn write_notification(&self, server: &str, message: &super::protocol::LoggingMessage) {
        let level = message.level.as_deref().unwrap_or("info");
        let logger = match message.logger.as_deref() {
            Some(name) if !name.is_empty() => format!(" {name}:"),
            _ => String::new(),
        };
        let data = match message.data.as_ref() {
            Some(JsonValue::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => String::new(),
        };
        let data = data.replace("\r\n", "\n").replace('\n', "\n    ");
        self.write_line(&format!("[{server}] {level}{logger} {data}"));
    }

    /// Rotate to `<path>.<unix-seconds>[-N]` (the caller holds the append
    /// mutex). The handle is dropped first so no write can land in the renamed
    /// file, and the generation is bumped so an opener that raced this rename
    /// cannot install the stale path. Every generation is kept (user decision
    /// 2026-10-21: logs are not deleted).
    fn rotate_locked(&self, state: &mut LogState) {
        state.file = None;
        state.size = 0;
        state.generation = state.generation.wrapping_add(1);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        for attempt in 0..1000u32 {
            let mut rotated = self.path.clone().into_os_string();
            if attempt == 0 {
                rotated.push(format!(".{stamp}"));
            } else {
                rotated.push(format!(".{stamp}.{attempt}"));
            }
            let rotated = Path::new(&rotated);
            if rotated.exists() {
                continue;
            }
            let _ = std::fs::rename(&self.path, rotated);
            return;
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` timestamp from the wall clock.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = now / 86_400;
    let seconds_of_day = now % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Days-since-epoch to (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
