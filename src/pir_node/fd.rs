//! Open-descriptor table for `fs.open`/`read`/`write`/`close`.
//!
//! Ported verbatim from `src/extensions/runtime/state.rs` so the fs module can
//! live entirely in this crate. One table per [`crate::pir_node::NodeRuntime`]; entries
//! are generation-scoped so a `/reload` closes exactly the previous runtime's
//! descriptors.

use std::collections::HashMap;
use std::path::PathBuf;

/// Hard cap on simultaneously open `fs.open` descriptors per runtime.
///
/// The JS side can open descriptors in a loop and never close them; without a
/// cap that exhausts the host process's OS descriptor budget (and every later
/// host operation with `EMFILE`). 256 is Node's common soft-limit scale while
/// staying far below a constrained device's process-wide descriptor budget;
/// the table is shared by every extension of one runtime, so the cap is the
/// runtime's total, not one extension's.
pub const MAX_OPEN_FDS: usize = 256;

pub struct FdTable {
    next_fd: i32,
    entries: HashMap<i32, FdEntry>,
}

/// One open descriptor: the OS handle plus the host-tracked sequential
/// offset (`fs.readSync(fd, buf, ..., null)` reads from here).
pub struct FdEntry {
    pub file: std::fs::File,
    /// Path text for Node-shaped error messages.
    pub path: PathBuf,
    /// Node `a`/`a+` semantics: writes append regardless of the offset.
    append: bool,
    /// Sequential offset; explicit positions never move it (Node/pread
    /// behavior), absent positions read/write from here and advance it.
    position: u64,
    /// Generation of the runtime that opened this descriptor (see
    /// [`FdTable`]).
    generation: u64,
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FdTable {
    pub fn new() -> Self {
        Self {
            // 0/1/2 are the stdio descriptors in Node; the first host fd is 3.
            next_fd: 3,
            entries: HashMap::new(),
        }
    }

    /// Register `file` and return its descriptor. Numbers are monotonic
    /// (never reused), so a stale descriptor can only fail with EBADF.
    ///
    /// `generation` is the OPENING runtime's own key: the descriptor is
    /// released by that runtime's teardown and by nothing else.
    ///
    /// `Err(())` when [`MAX_OPEN_FDS`] descriptors are already open; the
    /// caller maps that to Node's `EMFILE` before the file handle is stored
    /// (the unregistered `file` drops with the caller's error path).
    #[allow(clippy::result_unit_err)] // () is the deliberate "table full" signal.
    pub fn insert(
        &mut self,
        file: std::fs::File,
        path: PathBuf,
        append: bool,
        generation: u64,
    ) -> Result<i32, ()> {
        if self.entries.len() >= MAX_OPEN_FDS {
            return Err(());
        }
        let fd = self.next_fd;
        self.next_fd += 1;
        self.entries.insert(
            fd,
            FdEntry {
                file,
                path,
                append,
                position: 0,
                generation,
            },
        );
        Ok(fd)
    }

    pub fn get_mut(&mut self, fd: i32) -> Option<&mut FdEntry> {
        self.entries.get_mut(&fd)
    }

    /// Close: the returned entry drops (and releases the handle) with the
    /// caller's binding.
    pub fn remove(&mut self, fd: i32) -> Option<FdEntry> {
        self.entries.remove(&fd)
    }

    /// Close every descriptor owned by `generation` (runtime teardown /
    /// `/reload`). Scoped by generation: a reload's old generation and its
    /// successor share this table, and the old `Drop` may run after the
    /// successor has already opened descriptors of its own.
    pub fn clear_generation(&mut self, generation: u64) {
        self.entries
            .retain(|_, entry| entry.generation != generation);
    }

    /// Number of live descriptors (test seam for the generation release).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl FdEntry {
    /// Read up to `length` bytes; `position` absent = sequential (advances
    /// the tracked offset), explicit = a positioned read (offset untouched).
    pub fn read(&mut self, length: usize, position: Option<u64>) -> std::io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        let at = position.unwrap_or(self.position);
        let read = positioned_read(&self.file, &mut buffer, at)?;
        buffer.truncate(read);
        if position.is_none() {
            self.position = at + read as u64;
        }
        Ok(buffer)
    }

    /// Write `data`; `position` absent = sequential. Append-mode descriptors
    /// write at EOF regardless (Node's documented `a` flag behavior).
    pub fn write(&mut self, data: &[u8], position: Option<u64>) -> std::io::Result<usize> {
        let at = if self.append {
            self.file.metadata()?.len()
        } else {
            position.unwrap_or(self.position)
        };
        positioned_write(&self.file, data, at)?;
        if self.append || position.is_none() {
            self.position = at + data.len() as u64;
        }
        Ok(data.len())
    }
}

/// Positioned read that does not move the OS file pointer (`pread` /
/// `seek_read`); the tracked offset is the only sequential cursor.
#[cfg(windows)]
fn positioned_read(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(unix)]
fn positioned_read(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

/// Positioned write-all (`pwrite` / a `seek_write` loop — the Windows trait
/// has no `write_all_at`).
#[cfg(windows)]
fn positioned_write(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let n = std::os::windows::fs::FileExt::seek_write(
            file,
            &data[written..],
            offset + written as u64,
        )?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write the whole buffer",
            ));
        }
        written += n;
    }
    Ok(())
}

#[cfg(unix)]
fn positioned_write(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}
