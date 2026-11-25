//! Deterministic-deflate embedding for the extension-host JS shims.
//!
//! `scripts/gen_shims_deflate.py` writes `assets/shims/<name>.js.txt.deflate`
//! beside the raw `src/extensions/api/js/*.js.txt` sources. The API modules
//! `include_bytes!` the blobs (not the sources) and expose each one as a
//! [`Shim`]. [`Shim::source`] caches the inflation for the process (the
//! prelude and the globals bundle are needed by every runtime construction),
//! while [`Shim::inflate`] returns a fresh exactly-sized `String` for the
//! module loader: a virtual module's decoded source is dead once QuickJS has
//! compiled it, and the boot sweep imports every shimmed builtin, so keeping
//! all of them resident would pin ~2 MB of Rust heap for the session.
//!
//! The raw `.js.txt` files remain the source of truth: tests, the generator and
//! the SDK-surface auditors read them, and the golden test in `g_extensions`
//! pins the exact pre-deflate bytes with SHA-256. The blobs must live outside
//! `src/` because the source-hygiene gate reads every file under `src/` as
//! UTF-8 text.

use std::fmt;
use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::OnceLock;

/// `Write` sink that only counts bytes: the first inflation pass sizes the
/// exact-capacity buffer without retaining (or growing) a payload.
#[derive(Default)]
struct CountingSink {
    bytes: u64,
}

impl Write for CountingSink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One embedded, deflate-compressed JS shim.
pub struct Shim {
    name: &'static str,
    deflated: &'static [u8],
    source: OnceLock<String>,
}

impl Shim {
    /// `name` is the raw source file name (diagnostics and the golden test);
    /// `deflated` is the zlib stream embedded with `include_bytes!`.
    pub const fn new(name: &'static str, deflated: &'static [u8]) -> Self {
        Self {
            name,
            deflated,
            source: OnceLock::new(),
        }
    }

    /// Raw source file name, e.g. `prelude.js.txt`.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The embedded zlib bytes, exactly as `include_bytes!` produced them.
    pub fn deflated(&self) -> &'static [u8] {
        self.deflated
    }

    /// Inflate the blob without touching the cache. Used by the byte-identity
    /// test and the boot-cost measurement.
    ///
    /// The inflated `String` is cached for the process, so its capacity is
    /// part of the resident cost: the old pre-size (`deflated.len() * 4`,
    /// ~2.0 MB for the ~1.1 MB prelude) kept the overshoot live forever.
    /// A zlib stream carries no decoded length, so `read_to_end` has to grow
    /// (and re-copy) the buffer several times; `shrink_to_fit` then re-copies
    /// it once more. A first pass counts the decoded bytes into a sink with
    /// no payload allocation, and the second pass decodes into an
    /// exact-capacity `Vec` (`read_exact`, no growth, no shrink copy). The
    /// decoded bytes are unchanged and the cached buffer is exactly sized.
    pub fn inflate(&self) -> std::io::Result<String> {
        let mut counter = CountingSink::default();
        std::io::copy(
            &mut flate2::read::ZlibDecoder::new(self.deflated),
            &mut counter,
        )?;
        let length = usize::try_from(counter.bytes).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "embedded shim expands beyond the platform address space",
            )
        })?;
        let mut bytes = vec![0u8; length];
        let mut decoder = flate2::read::ZlibDecoder::new(self.deflated);
        decoder.read_exact(&mut bytes)?;
        let mut trailing = [0u8; 1];
        if decoder.read(&mut trailing)? != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "embedded shim changed length between inflate passes",
            ));
        }
        String::from_utf8(bytes).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.utf8_error())
        })
    }

    /// The JS source, inflated on first use and cached for the process.
    pub fn source(&self) -> &str {
        self.source.get_or_init(|| {
            self.inflate().unwrap_or_else(|error| {
                panic!("embedded shim `{}` failed to inflate: {error}", self.name)
            })
        })
    }
}

impl Deref for Shim {
    type Target = str;

    fn deref(&self) -> &str {
        self.source()
    }
}

impl fmt::Display for Shim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.source())
    }
}
