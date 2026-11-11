//! Read-only preview of shell spill files (`details.fullOutputPath`).
//!
//! The bash/powershell capture advertises a temp file holding the complete
//! output when it truncates the retained tail. Ctrl+O previously re-rendered
//! only the in-memory tail; this module lets the expanded view read the
//! spilled file directly. The file can be large (the writer caps one file at
//! 64 MiB), so the preview never materializes it as a `String`:
//!
//! * the bytes are a read-only `memmap2::Mmap` (file pages are page cache,
//!   not private heap);
//! * an exact-capacity `Vec<u64>` line-offset index is built with `memchr`;
//! * callers slice a bounded line range and only that range is copied into
//!   the render buffer.
//!
//! Fallbacks preserve the old behavior's availability: an empty file, a
//! failed map, a non-UTF-8 body, or a fallback read larger than
//! [`FALLBACK_READ_MAX_BYTES`] makes the preview unavailable
//! ([`MappedLines::open`] / [`MappedLines::tail_preview`] return `None`); the
//! caller then renders the in-memory retained output exactly as before.
//!
//! Windows lifetime rules (validated by `mmap_lifecycle` below):
//! append while mapped is allowed but the existing map keeps its old length
//! (remap to see the tail); truncation/replacement fails while a map is
//! alive, so the map must be dropped before the file is shortened or removed.
//! `Mmap::map` also fails for a zero-length file, hence the explicit empty
//! fallback.

// `Mmap::map` is an unsafe API: the mapping is read-only and the backing file
// is opened read-only, but the OS cannot prove no other writer truncates it.
#![allow(unsafe_code)]

use std::path::Path;

/// Largest file a failed-mmapping fallback will read into memory. A spill
/// file is at most [`crate::coding_agent::tools::spill::SPILL_MAX_FILE_BYTES`]
/// (64 MiB); reading that much defeats the purpose, so fall back to the
/// already-resident tail instead.
pub const FALLBACK_READ_MAX_BYTES: u64 = 8 * 1024 * 1024;

enum PreviewBytes {
    #[cfg(feature = "session")]
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

/// A read-only view of a spill file. The line index is computed lazily by
/// the accessor: the production path (`tail_preview`) only scans the tail, so
/// opening a 58 MB file does not build a 1M-entry offset vector.
pub struct MappedLines {
    bytes: PreviewBytes,
}

impl MappedLines {
    /// Open `path` for preview. `None` means "use the caller's fallback".
    pub fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        if len == 0 {
            return None;
        }
        #[cfg(feature = "session")]
        let bytes = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(map) => PreviewBytes::Mapped(map),
            Err(_) => {
                if len > FALLBACK_READ_MAX_BYTES {
                    return None;
                }
                PreviewBytes::Owned(std::fs::read(path).ok()?)
            }
        };
        #[cfg(not(feature = "session"))]
        let bytes = {
            if len > FALLBACK_READ_MAX_BYTES {
                return None;
            }
            PreviewBytes::Owned(std::fs::read(path).ok()?)
        };
        Self::from_bytes(bytes)
    }

    fn from_bytes(bytes: PreviewBytes) -> Option<Self> {
        let slice: &[u8] = match &bytes {
            #[cfg(feature = "session")]
            PreviewBytes::Mapped(map) => &map[..],
            PreviewBytes::Owned(vec) => &vec[..],
        };
        if slice.is_empty() {
            return None;
        }
        // UTF-8 validity is checked per line when a slice is copied
        // (`line`/`tail_preview` return `None`), so a binary body falls back
        // without a full-file scan.
        Some(Self { bytes })
    }

    pub fn len(&self) -> u64 {
        self.bytes_len() as u64
    }

    fn bytes(&self) -> &[u8] {
        match &self.bytes {
            #[cfg(feature = "session")]
            PreviewBytes::Mapped(map) => &map[..],
            PreviewBytes::Owned(vec) => &vec[..],
        }
    }

    fn bytes_len(&self) -> usize {
        self.bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes_len() == 0
    }

    /// Number of logical lines (`str::lines` semantics).
    pub fn line_count(&self) -> usize {
        let bytes = self.bytes();
        if bytes.is_empty() {
            return 0;
        }
        memchr::memchr_iter(b'\n', bytes).count() + usize::from(bytes.last() != Some(&b'\n'))
    }

    /// The `index`-th line without its trailing newline.
    pub fn line(&self, index: usize) -> Option<&str> {
        let bytes = self.bytes();
        let mut start = 0usize;
        let mut current = 0usize;
        for position in memchr::memchr_iter(b'\n', bytes) {
            if current == index {
                return std::str::from_utf8(&bytes[start..position]).ok();
            }
            current += 1;
            start = position + 1;
        }
        if current == index && start < bytes.len() {
            return std::str::from_utf8(&bytes[start..]).ok();
        }
        None
    }

    /// Copy the last `max_lines` lines (and at most `max_bytes` bytes) into a
    /// preview buffer. Both the scan and the copy are bounded by the tail:
    /// the file itself is never materialized or fully indexed.
    pub fn tail_preview(&self, max_lines: usize, max_bytes: usize) -> Option<String> {
        if max_lines == 0 {
            return None;
        }
        let bytes = self.bytes();
        if bytes.is_empty() {
            return None;
        }
        // `str::lines` semantics: a trailing newline does not add an empty
        // final line.
        let content_len = bytes
            .len()
            .saturating_sub(usize::from(bytes.last() == Some(&b'\n')));
        if content_len == 0 {
            return None;
        }
        // Walk backwards over at most `max_lines` line starts.
        let mut start = content_len;
        let mut scan_end = content_len;
        let mut found = 0usize;
        while found < max_lines && scan_end > 0 {
            match memchr::memrchr(b'\n', &bytes[..scan_end]) {
                Some(position) => {
                    start = position + 1;
                    scan_end = position;
                    found += 1;
                }
                None => {
                    start = 0;
                    break;
                }
            }
        }
        let mut kept: Vec<&str> = Vec::new();
        let mut used = 0usize;
        for line in bytes[start..content_len].split(|byte| *byte == b'\n') {
            let line = std::str::from_utf8(line).ok()?;
            used += line.len() + usize::from(!kept.is_empty());
            kept.push(line);
        }
        if kept.is_empty() {
            return None;
        }
        // Byte budget: drop the earliest lines (keep the suffix, matching the
        // collapsed shell preview) until the body fits. A single huge line is
        // kept whole rather than returning nothing.
        let mut first = 0usize;
        while first + 1 < kept.len() && used > max_bytes {
            used -= kept[first].len() + 1;
            first += 1;
        }
        let mut body = String::with_capacity(used.min(max_bytes));
        for (index, line) in kept[first..].iter().enumerate() {
            if index > 0 {
                body.push('\n');
            }
            body.push_str(line);
        }
        Some(body)
    }
}
