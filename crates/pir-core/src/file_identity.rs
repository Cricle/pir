//! Filesystem identity: "is the thing named by this path the same file?".
//!
//! Length, mtime and even a canonical path cannot answer that question. A file
//! replaced by an atomic rename keeps every one of them (the canonical path is
//! *identical* by construction), and a session transcript rewritten with only
//! its header `version` digit flipped has exactly the same byte length as its
//! predecessor. The only reliable answer is the pair the OS assigns per inode:
//! `(dev, ino)` on Unix, `(volume serial number, file index)` on Windows.
//!
//! # Why this module exists
//!
//! `std` exposes neither pair portably. Unix has it on stable
//! `std::os::unix::fs::MetadataExt`; Windows does **not** — `volume_serial_number`
//! and `file_index` are still unstable `windows_by_handle` accessors, so the
//! only stable route is `GetFileInformationByHandleEx(FileIdInfo)` through
//! `windows-sys`. That is the whole reason for the platform fork here, and it
//! is confined to this module so the rest of the crate keeps its single-path
//! habit:
//!
//! * [`identity_of`] — identity from an OPEN handle (never re-opens the path);
//! * [`identity_at`] — identity of whatever a PATH currently names, without
//!   data access (an attribute-only open, see [`WINDOWS`] below).
//!
//! Two callers share it: the session manager's append-cache staleness check
//! (which must never write through a handle whose file was replaced) and the
//! coding agent's spill-file exception (which must not admit a re-created file
//! that merely reuses a registered name).
//!
//! # The Windows probe is attribute-only on purpose
//!
//! [`identity_at`] opens with `FILE_READ_ATTRIBUTES`, never a data read. On
//! hosts with a filter driver (Defender real-time protection) the FIRST
//! data-read open of a just-written file costs ~17 ms while the
//! attribute-only probe costs ~60 µs — measured on the endurance box,
//! 2026-11-13. The identity returned is the same `BY_HANDLE_FILE_INFORMATION`
//! pair an ordinary `File::open` would produce, so the staleness check (about
//! the PATH naming a different file) keeps its exact semantics.
//!
//! Platforms with neither pair return `None`. Every caller treats that as
//! "identity unavailable" and falls back to its own conservative answer — for
//! the append cache, reopen the path every time; for the spill exception,
//! refuse the path.

use std::path::Path;

/// The OS's per-inode identifier for one file.
///
/// `Copy` and cheap to compare: callers store it beside an open handle and
/// check it on every write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIdentity {
    /// Unix `(dev, ino)`: the device and inode numbers.
    #[cfg(unix)]
    Unix { dev: u64, ino: u64 },
    /// Windows `(volume serial number, 128-bit file id)`.
    ///
    /// The 128-bit id is `FILE_ID_INFO`'s `FileId` — the MFT record number
    /// plus its **sequence number** — not the 64-bit `nFileIndex*` pair from
    /// `BY_HANDLE_FILE_INFORMATION`. That distinction is load-bearing: NTFS
    /// reuses a freed MFT record immediately, so a delete-then-create
    /// commonly reports the SAME 64-bit file index (measured: the same value
    /// for both files, and the same `Metadata::created()` timestamp too). The
    /// sequence number is what the filesystem bumps on reuse, which is why
    /// only the 128-bit form can tell the two files apart (SPILL-1).
    ///
    /// `volume_serial_number` stays `u64` because that is the width
    /// `FILE_ID_INFO` reports; it is only ever compared, never interpreted.
    #[cfg(windows)]
    Windows {
        volume_serial_number: u64,
        file_id: [u8; 16],
    },
}

impl FileIdentity {
    /// Identity from path-level metadata, where the OS exposes it there.
    ///
    /// Unix is the only such platform: `dev`/`ino` are stable `MetadataExt`
    /// accessors and cost nothing beyond the `stat` the caller already did.
    /// Windows gets `None` here — its metadata carries no file index, which is
    /// why [`identity_at`] has to open the path.
    #[cfg(unix)]
    pub fn of_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt as _;
        Some(Self::Unix {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

/// Identity of an OPEN file, straight off its handle.
///
/// Preferred over [`identity_at`] whenever a handle is already held: a handle
/// is immune to a path swap by construction, so the answer describes the file
/// the caller will actually write to.
///
/// Unix reads the handle's metadata. Windows probes `FILE_ID_INFO`
/// (no stable `MetadataExt` accessor exists, see the module docs). A failure
/// returns `None`, which every caller treats as "reopen and retry", never as
/// "same file".
#[must_use]
pub fn identity_of(file: &std::fs::File) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        let metadata = file.metadata().ok()?;
        FileIdentity::of_metadata(&metadata)
    }
    #[cfg(windows)]
    {
        windows::identity(file).map(|(volume_serial_number, file_id)| FileIdentity::Windows {
            volume_serial_number,
            file_id,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        None
    }
}

/// Identity of whatever `path` currently names, without reading its data.
///
/// Windows opens an attribute-only handle and probes it (see the module docs
/// for the measured reason it must not be a data read); Unix just `stat`s.
/// `None` means the path does not exist, cannot be opened, or the platform has
/// no identity — the three cases callers deliberately treat the same way,
/// because in all of them the path cannot be proven to be the expected file.
#[must_use]
pub fn identity_at(path: &Path) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        let metadata = std::fs::metadata(path).ok()?;
        FileIdentity::of_metadata(&metadata)
    }
    #[cfg(windows)]
    {
        windows::identity_at(path).map(|(volume_serial_number, file_id)| FileIdentity::Windows {
            volume_serial_number,
            file_id,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// Win32 `FILE_ID_INFO` identity probe.
///
/// `std::os::windows::fs::MetadataExt::{volume_serial_number, file_index}` are
/// still unstable, so this is the stable way to ask. Isolated, documented
/// `unsafe` boundary: the crate denies `unsafe_code` everywhere else, and
/// `tests/unsafe_gate_allowlist.txt` pins the block count in this file.
///
/// `FILE_ID_INFO` and NOT the older `BY_HANDLE_FILE_INFORMATION` pair: that
/// pair's 64-bit file index is the bare MFT record number, which NTFS reuses
/// the moment the record is freed. Measured here, a file deleted and
/// re-created at one path reported the SAME 64-bit index AND the same
/// `Metadata::created()` timestamp, so neither could detect the replacement.
/// `FILE_ID_INFO.FileId` is 128 bits — record number plus the sequence number
/// the filesystem bumps on reuse — which does distinguish them.
#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]

    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    pub(super) fn identity(file: &std::fs::File) -> Option<(u64, [u8; 16])> {
        // SAFETY: `FILE_ID_INFO` is plain old data (integers and a byte
        // array); its all-zero bit pattern is valid, and the call below fills
        // the whole struct before any field is read.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is live (borrowed from `file`), `info` is a valid
        // out-parameter, and the size passed is exactly `info`'s size, so the
        // call cannot write past it.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                std::ptr::addr_of_mut!(info).cast::<std::ffi::c_void>(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        Some((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    /// Identity of the file currently named by `path`, WITHOUT data access.
    ///
    /// Share flags match `std::fs::File::open`'s (READ|WRITE|DELETE) so a
    /// concurrent appender or reader cannot make the probe fail where the
    /// plain open would have succeeded.
    pub(super) fn identity_at(path: &Path) -> Option<(u64, [u8; 16])> {
        let wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt as _;
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        // SAFETY: `wide` is NUL-terminated and outlives the call; the returned
        // handle is wrapped into a `File` (closed on drop) on every path.
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: `handle` came from `CreateFileW` and is owned here; the
        // `File` takes over the close.
        let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
        identity(&file)
    }
}
