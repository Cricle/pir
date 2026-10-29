//! Deterministic-deflate embedding for the extension-host JS shims.
//!
//! `scripts/gen_shims_deflate.py` writes `assets/shims/<name>.js.txt.deflate`
//! beside the raw `src/extensions/api/js/*.js.txt` sources. The API modules
//! `include_bytes!` the blobs (not the sources) and expose each one as a
//! [`Shim`], which inflates its zlib stream at most once per process, on first
//! use. The extension host boots by importing every shimmed builtin, so the
//! first boot pays the whole inflate cost (~3.3 MB decoded) and later uses read
//! the cached `String`.
//!
//! The raw `.js.txt` files remain the source of truth: tests, the generator and
//! the SDK-surface auditors read them, and the golden test in `g_extensions`
//! pins the exact pre-deflate bytes with SHA-256. The blobs must live outside
//! `src/` because the source-hygiene gate reads every file under `src/` as
//! UTF-8 text.

use std::fmt;
use std::io::Read;
use std::ops::Deref;
use std::sync::OnceLock;

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
    pub fn inflate(&self) -> std::io::Result<String> {
        // Shims compress at ~4.0x; pre-size the buffer to avoid regrowth.
        let mut text = String::with_capacity(self.deflated.len().saturating_mul(4));
        flate2::read::ZlibDecoder::new(self.deflated).read_to_string(&mut text)?;
        Ok(text)
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
