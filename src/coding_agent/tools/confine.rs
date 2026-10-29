//! Handle-relative confinement for the local file tools (`read`, `write`,
//! `edit`).
//!
//! `util::workspace_path` decides confinement by *name*: it canonicalizes the
//! workspace root and rejects symbolic links component by component with
//! `lstat`. Between that decision and the open, a path component can still be
//! replaced by a symbolic link (the classic resolve-then-open TOCTOU), which
//! would redirect `open` / `create_dir_all` / `create_new` / `rename` outside
//! the workspace.
//!
//! This module re-establishes the decision against open handles:
//!
//! * **Unix** opens the parent directory component by component with
//!   `openat(O_DIRECTORY|O_NOFOLLOW)` starting from an open descriptor on the
//!   canonical root, so a swapped component can never be traversed (the
//!   kernel resolves each name relative to the descriptor that was already
//!   verified). Missing parent directories are created with `mkdirat` and
//!   re-opened with `O_NOFOLLOW`; the temp file is created with
//!   `openat(O_CREAT|O_EXCL|O_NOFOLLOW)` and the replacement is
//!   `renameat(dirfd, temp, dirfd, target)`. Every step after the root open is
//!   descriptor-relative; reads use `openat(O_RDONLY|O_NOFOLLOW)`. There is no
//!   remaining path re-resolution window on this platform.
//! * **Windows** opens each component with `FILE_FLAG_OPEN_REPARSE_POINT` (a
//!   link is opened as a link, never followed), rejects reparse points and
//!   re-validates the handle's final path (`GetFinalPathNameByHandleW`)
//!   against the path the walk expects. Parent directories are created one
//!   component at a time and then verified through their handles, so a link
//!   planted before the create is rejected instead of traversed. File opens
//!   re-check the opened handle's parent directory before any byte is read
//!   (final-name aliases such as 8.3 names are ignored: only containment in
//!   the verified parent matters). The one remaining window is
//!   `DirHandle::replace`: Win32 has no descriptor-relative replace through
//!   `MoveFileExW`, so the rename takes the last handle-verified path. A
//!   directory swapped between that verification and the rename can redirect
//!   the replacement (narrow: the swap must win that exact interval; the temp
//!   file and target still share the verified spelling). No file contents are
//!   read or written through the redirect — only the final rename would land
//!   elsewhere.
//! * Other platforms fall back to the path-based behavior of
//!   `util::write_atomically` (no handle-relative primitives are guaranteed
//!   there).
//!
//! The original symlink-escape check (`reject_symlink_components`) stays in
//! place: it produces the model-facing errors and rejects the ordinary,
//! non-racy cases before any filesystem mutation happens.
//!
//! # Replacement durability and metadata semantics
//!
//! [`ConfinedTarget::write_atomically`] writes a same-directory temp file,
//! flushes it, calls `sync_all`, and renames it over the target. The
//! `sync_all` is **unconditional**: every successful local write is on stable
//! storage before the tool reports success. That is a deliberate safety
//! choice, not a tunable: the settings schema has no write-durability knob
//! (adding one would let a configuration file trade away crash safety), so the
//! cost is paid on every call. On a network filesystem this can dominate the
//! write latency; the alternative — a half-written file after a power loss —
//! is worse.
//!
//! A target with a link count above one is rewritten **in place** instead of
//! being replaced by rename. A rename installs a new inode at one directory
//! entry and leaves every other hard link pointing at the old bytes, so an
//! atomic replacement cannot update all links; the inode itself has to change.
//! In-place writing preserves the inode, permission bits, owner, ACLs, extended
//! attributes, and every hard link, but it cannot be atomic: a concurrent
//! reader (or a crash or I/O failure mid-write) can observe a mix of old and
//! new bytes. Chunks are written starting at offset zero and the file is
//! truncated only after the whole payload was written, so a failure does not
//! first shorten the target. This is the best available tradeoff on POSIX and
//! Win32; there is no primitive that atomically replaces the contents of one
//! inode.
//!
//! For the ordinary temp+rename path, only the permission bits are copied to
//! the new inode (best effort). ACLs and extended attributes are **not**
//! preserved: on Unix they belong to the old inode, and on Windows the new
//! file keeps the ACL inherited from its directory rather than the replaced
//! file's explicit entries. Replaying arbitrary ACLs/xattrs is not portable
//! (and frequently requires privileges), so this limitation is documented
//! rather than emulated. The rename also deliberately breaks hard links,
//! which is why a linked target takes the in-place path above.
//!
//! Compiled under `any(tool-read, tool-write, tool-edit, tui)`: the file
//! tools and the TUI edit preview read through the same confinement. A build
//! with none of them has no reader/mutation tools, so the helpers are
//! unreachable there.
#![cfg_attr(
    not(all(feature = "tool-read", feature = "tool-write", feature = "tool-edit")),
    allow(dead_code)
)]

use std::io;
use std::path::Path;

use crate::ai::types::AbortSignal;

use super::util::{atomic_temp_name, cancellable_for_write};

/// Whether `error` is the handle walk's own confinement refusal rather than a
/// real operating-system access denial.
///
/// The walk constructs its refusals (path outside the root, reparse point /
/// symbolic link traversal, a resolved path that leaves the verified
/// directory) with [`io::Error::new`] and the `PermissionDenied` kind, so they
/// carry no OS error code. A kernel or host denial — `EACCES` on Unix,
/// `ERROR_ACCESS_DENIED` on Windows — comes from `last_os_error()` (or a
/// `std::fs` call) and therefore always carries one. Callers use this split to
/// keep the confinement wording for policy refusals and to report an OS denial
/// accurately, so the model fixes permissions instead of hunting for another
/// path.
pub(super) fn is_confinement_refusal(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied && error.raw_os_error().is_none()
}

/// Render the handle walk's refusals for the model.
///
/// A *policy* refusal (the walk's own `PermissionDenied` errors: path outside
/// the root, reparse point or symbolic link traversal) keeps the wording the
/// name-based [`workspace_path`](super::util::workspace_path) check uses, so a
/// rejection is indistinguishable to the model regardless of which check
/// caught it. A real OS denial (`EACCES` / Windows access-denied) has the same
/// `PermissionDenied` kind but must not claim the path escapes: the path is
/// inside the workspace, and the model needs to fix permissions instead of
/// hunting for another path.
///
/// Shared by `read`, `write` and `edit` (each leaf can be compiled alone).
pub(super) fn confinement_error(requested: &str, error: &io::Error) -> String {
    if !is_confinement_refusal(error) {
        return format!(
            "Path '{}': permission denied (the OS denied access; check the file or directory permissions)",
            requested
        );
    }
    let message = error.to_string();
    if message.contains("reparse point") || message.contains("symbolic link") {
        format!("Path '{}' must not traverse a symbolic link", requested)
    } else {
        format!("Path '{}' escapes the workspace root", requested)
    }
}

/// What the replacement needs to know about an existing target before it
/// chooses a strategy: the permission bits to copy onto a fresh temp file (only
/// for a regular file; a link or special file must not lend its mode to the
/// temp) and the number of directory entries sharing the inode.
struct TargetInfo {
    permissions: Option<std::fs::Permissions>,
    hard_links: u64,
}

/// A target file inside the workspace whose parent directory is held open (or,
/// on Windows, verified through a handle).
pub(super) struct ConfinedTarget {
    dir: imp::DirHandle,
    name: std::ffi::OsString,
}

impl ConfinedTarget {
    /// Resolve the workspace-relative `relative` (the components the name
    /// check already validated) into a handle anchored at its parent
    /// directory. With `create_parents`, missing components are created
    /// handle-relative.
    pub(super) fn open(root: &Path, relative: &Path, create_parents: bool) -> io::Result<Self> {
        let Some(name) = relative.file_name() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path names the workspace root itself",
            ));
        };
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let dir = imp::DirHandle::open(root, parent, create_parents)?;
        Ok(Self {
            dir,
            name: name.to_os_string(),
        })
    }

    /// Open the target for reading without following a symbolic link in the
    /// final component.
    pub(super) fn open_read(&self) -> io::Result<std::fs::File> {
        self.dir.open_file(&self.name, true)
    }

    /// Replace the target with `content` durably.
    ///
    /// For an ordinary file this writes a same-directory `create_new` temp
    /// file, flushes and `sync_all`s it, then renames it over the target
    /// (atomic replacement; permissions of an existing target are copied to
    /// the temp file where the platform supports it). For a file with more
    /// than one hard link it rewrites the inode in place instead, so every
    /// link keeps seeing the same bytes; see the module docs for the
    /// atomicity/metadata tradeoff.
    ///
    /// `Ok(true)` is a completed replacement, `Ok(false)` an abort observed
    /// between chunks.
    pub(super) async fn write_atomically(
        &self,
        content: &[u8],
        signal: Option<&AbortSignal>,
        chunk_bytes: usize,
    ) -> io::Result<bool> {
        use tokio::io::AsyncWriteExt;

        if signal.is_some_and(AbortSignal::is_aborted) {
            return Ok(false);
        }
        let existing = self.dir.target_info(&self.name).ok();
        if existing.as_ref().is_some_and(|info| info.hard_links > 1) {
            return self.write_in_place(content, signal, chunk_bytes).await;
        }
        let original_permissions = existing.and_then(|info| info.permissions);
        let temp_name = atomic_temp_name();
        let temp_os = std::ffi::OsStr::new(&temp_name);
        let temp = self.dir.create(temp_os, original_permissions.as_ref())?;
        let mut file = tokio::fs::File::from_std(temp);

        let mut failure: Option<io::Error> = None;
        let mut aborted = false;
        for chunk in content.chunks(chunk_bytes.max(1)) {
            match cancellable_for_write(signal, file.write_all(chunk)).await {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    failure = Some(error);
                    break;
                }
                None => {
                    aborted = true;
                    break;
                }
            }
        }
        if failure.is_none() && !aborted {
            match cancellable_for_write(signal, file.flush()).await {
                Some(Ok(())) => {}
                Some(Err(error)) => failure = Some(error),
                None => aborted = true,
            }
        }
        if failure.is_none() && !aborted {
            match cancellable_for_write(signal, file.sync_all()).await {
                Some(Ok(())) => {}
                Some(Err(error)) => failure = Some(error),
                None => aborted = true,
            }
        }
        drop(file);

        if aborted {
            self.dir.remove(temp_os);
            return Ok(false);
        }
        if let Some(error) = failure {
            self.dir.remove(temp_os);
            return Err(error);
        }
        if signal.is_some_and(AbortSignal::is_aborted) {
            self.dir.remove(temp_os);
            return Ok(false);
        }
        if let Some(permissions) = original_permissions {
            // Best effort: a permission-copy failure must not turn a successful
            // content write into an orphaned temp file.
            self.dir.set_permissions(temp_os, permissions);
        }
        match self.dir.replace(temp_os, &self.name).await {
            Ok(()) => {
                self.dir.sync();
                Ok(true)
            }
            Err(error) => {
                self.dir.remove(temp_os);
                Err(error)
            }
        }
    }

    /// Rewrite an existing multi-link target through its own inode.
    ///
    /// The bytes are written from offset zero and the length is adjusted only
    /// after the whole payload was handed to the file, so an early failure
    /// cannot truncate the target first. The success path still flushes and
    /// `sync_all`s; the abort/failure paths necessarily leave whatever prefix
    /// was written (an in-place rewrite has no rename to fall back on). See
    /// the module docs.
    async fn write_in_place(
        &self,
        content: &[u8],
        signal: Option<&AbortSignal>,
        chunk_bytes: usize,
    ) -> io::Result<bool> {
        use tokio::io::AsyncWriteExt;

        if signal.is_some_and(AbortSignal::is_aborted) {
            return Ok(false);
        }
        let file = self.dir.open_write(&self.name)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut failure: Option<io::Error> = None;
        let mut aborted = false;
        for chunk in content.chunks(chunk_bytes.max(1)) {
            match cancellable_for_write(signal, file.write_all(chunk)).await {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    failure = Some(error);
                    break;
                }
                None => {
                    aborted = true;
                    break;
                }
            }
        }
        if failure.is_none() && !aborted {
            match cancellable_for_write(signal, file.flush()).await {
                Some(Ok(())) => {}
                Some(Err(error)) => failure = Some(error),
                None => aborted = true,
            }
        }
        if failure.is_none() && !aborted {
            match cancellable_for_write(signal, file.set_len(content.len() as u64)).await {
                Some(Ok(())) => {}
                Some(Err(error)) => failure = Some(error),
                None => aborted = true,
            }
        }
        if failure.is_none() && !aborted {
            match cancellable_for_write(signal, file.sync_all()).await {
                Some(Ok(())) => {}
                Some(Err(error)) => failure = Some(error),
                None => aborted = true,
            }
        }
        drop(file);
        if aborted {
            return Ok(false);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(true),
        }
    }
}

/// Read the whole target through its confined handle in chunks, checking the
/// abort signal between reads. `Ok(None)` means the signal fired.
pub(super) async fn read_confined(
    target: &ConfinedTarget,
    signal: Option<&AbortSignal>,
    max_bytes: u64,
    chunk_bytes: usize,
    over_limit: impl Fn(u64) -> String,
) -> io::Result<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt;

    if signal.is_some_and(AbortSignal::is_aborted) {
        return Ok(None);
    }
    let mut file = tokio::fs::File::from_std(target.open_read()?);
    let metadata = match super::util::cancellable(signal, file.metadata()).await {
        Some(metadata) => metadata?,
        None => return Ok(None),
    };
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            over_limit(metadata.len()),
        ));
    }
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; chunk_bytes.max(1)];
    loop {
        match super::util::cancellable(signal, file.read(&mut chunk)).await {
            Some(result) => match result? {
                0 => return Ok(Some(bytes)),
                read => {
                    let next_len = (bytes.len() as u64).saturating_add(read as u64);
                    if next_len > max_bytes {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            over_limit(next_len),
                        ));
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
            },
            None => return Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Unix: descriptor-relative walk
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    //! `openat`/`mkdirat`/`renameat`/`unlinkat` are the only way to make the
    //! walk descriptor-relative; this module is the sanctioned `unsafe`
    //! boundary for those calls (all arguments are NUL-terminated names that
    //! outlive the call, and every returned descriptor is owned immediately).
    #![allow(unsafe_code)]

    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    /// An open parent directory. Every operation below is performed relative
    /// to this descriptor, so re-resolving the path text cannot redirect it.
    pub(super) struct DirHandle {
        dir: OwnedFd,
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path component contains a NUL byte",
            )
        })
    }

    fn open_dir_at(parent: i32, name: &CString) -> io::Result<OwnedFd> {
        // SAFETY: `name` is a NUL-terminated string that outlives the call; a
        // non-negative return value is a freshly owned descriptor.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a valid descriptor returned by `openat` and not owned
        // anywhere else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    impl DirHandle {
        pub(super) fn open(root: &Path, parent: &Path, create: bool) -> io::Result<Self> {
            let root_name = c_name(root.as_os_str())?;
            // SAFETY: `root_name` is NUL-terminated and outlives the call.
            let root_fd = unsafe {
                libc::open(
                    root_name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if root_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `root_fd` is a valid descriptor returned by `open`.
            let mut current = unsafe { OwnedFd::from_raw_fd(root_fd) };
            for component in parent.components() {
                let Component::Normal(name) = component else {
                    if matches!(component, Component::CurDir) {
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "path is outside the workspace root",
                    ));
                };
                let name = c_name(name)?;
                match open_dir_at(current.as_raw_fd(), &name) {
                    Ok(next) => current = next,
                    Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                        // SAFETY: `name` is a single, NUL-terminated component
                        // and `current` is a live directory descriptor.
                        let created =
                            unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o777) };
                        if created != 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::AlreadyExists {
                                return Err(error);
                            }
                        }
                        current = open_dir_at(current.as_raw_fd(), &name)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(Self { dir: current })
        }

        /// Stat the target without opening it for reading, so an unreadable
        /// (write-only) file still reveals its link count and permission bits.
        /// `fstatat(.., AT_SYMLINK_NOFOLLOW)` never follows a swapped final
        /// component; a link (or any non-regular file) is reported with one
        /// link so the ordinary replacement path handles it.
        // `mode_t`/`nlink_t` are `u32`/`u64` on Linux but `u16` on macOS, so
        // the widening casts are required on macOS and identity casts on
        // Linux — where clippy's `unnecessary_cast` (denied in CI) fires.
        #[allow(clippy::unnecessary_cast)]
        pub(super) fn target_info(&self, name: &OsStr) -> io::Result<super::TargetInfo> {
            use std::os::unix::fs::PermissionsExt;

            let name = c_name(name)?;
            // SAFETY: `stat` is a plain out-parameter, `name` is NUL-terminated
            // and outlives the call, and `self.dir` is a live descriptor.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: same invariants as above; `AT_SYMLINK_NOFOLLOW` makes a
            // final-component link report the link itself.
            let result = unsafe {
                libc::fstatat(
                    self.dir.as_raw_fd(),
                    name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            let mode = stat.st_mode as u32;
            let is_regular = stat.st_mode & libc::S_IFMT == libc::S_IFREG;
            Ok(super::TargetInfo {
                permissions: is_regular.then(|| std::fs::Permissions::from_mode(mode & 0o7777)),
                hard_links: if is_regular { stat.st_nlink as u64 } else { 1 },
            })
        }

        pub(super) fn open_file(&self, name: &OsStr, nofollow: bool) -> io::Result<std::fs::File> {
            let name = c_name(name)?;
            let mut flags = libc::O_RDONLY | libc::O_CLOEXEC;
            if nofollow {
                flags |= libc::O_NOFOLLOW;
            }
            // SAFETY: `name` is NUL-terminated and `self.dir` is a live
            // directory descriptor; the returned descriptor is owned here.
            let fd = unsafe { libc::openat(self.dir.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor owned by this function.
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }

        /// Open an existing target for an in-place rewrite. `O_TRUNC` is
        /// deliberately absent: the caller writes the new bytes first and only
        /// then shortens the file, so a failed write does not lose the tail.
        pub(super) fn open_write(&self, name: &OsStr) -> io::Result<std::fs::File> {
            let name = c_name(name)?;
            // SAFETY: `name` is NUL-terminated, `self.dir` is a live directory
            // descriptor, and the returned descriptor is owned here.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor owned by this function.
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }

        pub(super) fn create(
            &self,
            name: &OsStr,
            permissions: Option<&std::fs::Permissions>,
        ) -> io::Result<std::fs::File> {
            use std::os::unix::fs::PermissionsExt;
            let name = c_name(name)?;
            let mode = permissions.map(|permissions| permissions.mode() & 0o7777);
            // SAFETY: `name` is NUL-terminated and `self.dir` is a live
            // directory descriptor; `O_EXCL` + `O_NOFOLLOW` guarantee a fresh
            // regular file (a pre-planted link makes the call fail).
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode.unwrap_or(0o600) as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor owned by this function.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            if let Some(permissions) = permissions {
                let _ = file.set_permissions(permissions.clone());
            }
            Ok(file)
        }

        pub(super) fn set_permissions(&self, name: &OsStr, permissions: std::fs::Permissions) {
            if let Ok(file) = self.open_file(name, true) {
                let _ = file.set_permissions(permissions);
            }
        }

        pub(super) async fn replace(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            let from = c_name(from)?;
            let to = c_name(to)?;
            // SAFETY: both names are NUL-terminated and both resolve relative
            // to the same live directory descriptor.
            let result = unsafe {
                libc::renameat(
                    self.dir.as_raw_fd(),
                    from.as_ptr(),
                    self.dir.as_raw_fd(),
                    to.as_ptr(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn remove(&self, name: &OsStr) {
            let Ok(name) = c_name(name) else {
                return;
            };
            // SAFETY: `name` is NUL-terminated and resolves relative to a live
            // directory descriptor.
            unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0);
            }
        }

        pub(super) fn sync(&self) {
            // SAFETY: `self.dir` is a live descriptor; fsync on a directory is
            // a best-effort durability step (errors are ignored).
            unsafe {
                libc::fsync(self.dir.as_raw_fd());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: reparse-point-rejecting walk + handle final-path validation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    //! Win32 has no reparse-point-aware safe open and no descriptor-relative
    //! create, so this module is the sanctioned `unsafe` boundary for the
    //! Windows walk — same posture as `src/env_daemon/sys/windows.rs`.
    #![allow(unsafe_code)]

    use std::io;
    use std::os::windows::io::FromRawHandle;
    use std::path::{Component, Path, PathBuf};

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_EXISTING, VOLUME_NAME_DOS,
    };

    /// The verified parent directory. Win32 has no descriptor-relative create,
    /// so this holds the *real* path reported by the last verified handle
    /// (`GetFinalPathNameByHandleW`), never the caller's spelling.
    pub(super) struct DirHandle {
        path: PathBuf,
    }

    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if wide.len() > i32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is too long",
            ));
        }
        Ok(wide)
    }

    fn open_directory(path: &Path) -> io::Result<std::fs::File> {
        let wide = wide(path)?;
        // SAFETY: `wide` is NUL-terminated and outlives the call;
        // FILE_FLAG_OPEN_REPARSE_POINT makes the call open the reparse point
        // itself instead of following it, and FILE_FLAG_BACKUP_SEMANTICS is
        // required to open a directory handle at all.
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `CreateFileW` returned a valid handle owned here.
        Ok(unsafe { std::fs::File::from_raw_handle(handle as _) })
    }

    fn attributes(file: &std::fs::File) -> io::Result<u32> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: `BY_HANDLE_FILE_INFORMATION` is plain old data; its all-zero
        // bit pattern is valid, and the call below fills every field used here.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is live (borrowed from `file`) and `info` is a
        // valid out-parameter.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.dwFileAttributes)
    }

    fn final_path(file: &std::fs::File) -> io::Result<PathBuf> {
        use std::os::windows::io::AsRawHandle;
        let handle = file.as_raw_handle() as HANDLE;
        let mut buffer = vec![0u16; 512];
        loop {
            // SAFETY: the handle is live and the buffer is a valid, writable
            // slice of `buffer.len()` UTF-16 units.
            let written = unsafe {
                GetFinalPathNameByHandleW(
                    handle,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            };
            if written == 0 {
                return Err(io::Error::last_os_error());
            }
            let written = written as usize;
            if written < buffer.len() {
                return Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..written])));
            }
            // The required size includes the terminator this time.
            buffer = vec![0u16; written + 1];
        }
    }

    /// Two paths name the same location (Windows compares case-insensitively;
    /// the callers always hold `\\?\`-style paths, so no normalization beyond
    /// separators is needed).
    fn same_path(left: &Path, right: &Path) -> bool {
        let render = |path: &Path| {
            path.to_string_lossy()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        render(left) == render(right)
    }

    fn verify_directory(path: &Path) -> io::Result<PathBuf> {
        let handle = open_directory(path)?;
        let attributes = attributes(&handle)?;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path component is a reparse point",
            ));
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "path component is not a directory",
            ));
        }
        final_path(&handle)
    }

    impl DirHandle {
        pub(super) fn open(root: &Path, parent: &Path, create: bool) -> io::Result<Self> {
            // Start from the canonical root's *real* path: everything above it
            // has already been resolved by `workspace_path`.
            let mut current = final_path(&open_directory(root)?)?;
            for component in parent.components() {
                let Component::Normal(name) = component else {
                    if matches!(component, Component::CurDir) {
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "path is outside the workspace root",
                    ));
                };
                let next = current.join(name);
                if create {
                    match std::fs::create_dir(&next) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                }
                let resolved = verify_directory(&next)?;
                if !same_path(&resolved, &next) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "path resolves outside the workspace directory",
                    ));
                }
                current = resolved;
            }
            Ok(Self { path: current })
        }

        /// Metadata for the write strategy: permission bits plus the Win32
        /// link count (`GetFileInformationByHandle.nNumberOfLinks`; the stable
        /// `MetadataExt` link-count accessor is still unstable). Opened with
        /// `FILE_READ_ATTRIBUTES` so a write-only file is still inspectable.
        pub(super) fn target_info(&self, name: &std::ffi::OsStr) -> io::Result<super::TargetInfo> {
            use std::os::windows::io::{AsRawHandle, FromRawHandle};

            let path = self.path.join(name);
            let wide = wide(&path)?;
            // SAFETY: `wide` is NUL-terminated and outlives the call;
            // FILE_READ_ATTRIBUTES asks only for metadata access and
            // FILE_FLAG_OPEN_REPARSE_POINT never follows a planted link.
            let handle: HANDLE = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    0,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `CreateFileW` returned a valid handle owned here.
            let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
            if attributes(&file)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "path must not traverse a symbolic link",
                ));
            }
            let resolved = final_path(&file)?;
            let parent_ok = resolved
                .parent()
                .is_some_and(|parent| same_path(parent, &self.path));
            if !parent_ok {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "path resolves outside the workspace directory",
                ));
            }
            // SAFETY: `BY_HANDLE_FILE_INFORMATION` is plain old data; all-zero
            // is a valid value and the call below fills it.
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: the handle is live (borrowed from `file`) and `info` is a
            // valid out-parameter.
            let ok =
                unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let permissions = file.metadata()?.permissions();
            let is_regular = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0;
            Ok(super::TargetInfo {
                permissions: is_regular.then_some(permissions),
                hard_links: if is_regular {
                    u64::from(info.nNumberOfLinks)
                } else {
                    1
                },
            })
        }

        pub(super) fn open_file(
            &self,
            name: &std::ffi::OsStr,
            nofollow: bool,
        ) -> io::Result<std::fs::File> {
            use std::os::windows::fs::OpenOptionsExt;
            let path = self.path.join(name);
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            if nofollow {
                options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            }
            let file = options.open(&path)?;
            if nofollow {
                if attributes(&file)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "path must not traverse a symbolic link",
                    ));
                }
                // The final component is checked above, but Win32 opens the
                // full path: a parent directory swapped after the walk can
                // still redirect this open. Re-validate through the handle
                // before any byte is read; a mismatch is refused.
                let resolved = final_path(&file)?;
                let parent_ok = resolved
                    .parent()
                    .is_some_and(|parent| same_path(parent, &self.path));
                if !parent_ok {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "path resolves outside the workspace directory",
                    ));
                }
            }
            Ok(file)
        }

        /// Open an existing target for an in-place rewrite. No truncation:
        /// the caller writes the new bytes first and adjusts the length after.
        pub(super) fn open_write(&self, name: &std::ffi::OsStr) -> io::Result<std::fs::File> {
            use std::os::windows::fs::OpenOptionsExt;
            let path = self.path.join(name);
            let mut options = std::fs::OpenOptions::new();
            options
                .write(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            let file = options.open(&path)?;
            if attributes(&file)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "path must not traverse a symbolic link",
                ));
            }
            let resolved = final_path(&file)?;
            let parent_ok = resolved
                .parent()
                .is_some_and(|parent| same_path(parent, &self.path));
            if !parent_ok {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "path resolves outside the workspace directory",
                ));
            }
            Ok(file)
        }

        pub(super) fn create(
            &self,
            name: &std::ffi::OsStr,
            _permissions: Option<&std::fs::Permissions>,
        ) -> io::Result<std::fs::File> {
            let path = self.path.join(name);
            let wide = wide(&path)?;
            // SAFETY: `wide` is NUL-terminated and outlives the call;
            // CREATE_NEW refuses a pre-planted name (including a link), and
            // FILE_FLAG_OPEN_REPARSE_POINT keeps the open from following one.
            let handle: HANDLE = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_WRITE_DATA,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                    0,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `CreateFileW` returned a valid handle owned here.
            let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
            // Re-validate through the handle: a directory component swapped
            // between the walk and this create would land elsewhere.
            let resolved = final_path(&file)?;
            let expected = self.path.join(name);
            if !same_path(&resolved, &expected) {
                drop(file);
                // Best effort: the stray file (created through the swapped
                // component) is not ours to keep.
                let _ = std::fs::remove_file(&resolved);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "created file resolves outside the verified directory",
                ));
            }
            Ok(file)
        }

        pub(super) fn set_permissions(
            &self,
            name: &std::ffi::OsStr,
            permissions: std::fs::Permissions,
        ) {
            let _ = std::fs::set_permissions(self.path.join(name), permissions);
        }

        pub(super) async fn replace(
            &self,
            from: &std::ffi::OsStr,
            to: &std::ffi::OsStr,
        ) -> io::Result<()> {
            // Win32 has no descriptor-relative replace: `MoveFileExW` (through
            // `std::fs::rename`) takes full paths, so the verified directory
            // path is used and the retry loop absorbs transient scanners.
            let from = self.path.join(from);
            let to = self.path.join(to);
            let mut delay = std::time::Duration::from_millis(10);
            for attempt in 0..8 {
                match tokio::fs::rename(&from, &to).await {
                    Ok(()) => return Ok(()),
                    Err(error) if attempt < 7 && retryable(&error) => {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::other("atomic replacement failed after retries"))
        }

        pub(super) fn remove(&self, name: &std::ffi::OsStr) {
            let _ = std::fs::remove_file(self.path.join(name));
        }

        pub(super) fn sync(&self) {}
    }

    fn retryable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock | io::ErrorKind::Other
        )
    }
}

// ---------------------------------------------------------------------------
// Fallback: other platforms keep the path-based behavior
// ---------------------------------------------------------------------------

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};

    pub(super) struct DirHandle {
        dir: PathBuf,
    }

    impl DirHandle {
        pub(super) fn open(root: &Path, parent: &Path, create: bool) -> io::Result<Self> {
            let dir = root.join(parent);
            if create {
                std::fs::create_dir_all(&dir)?;
            }
            Ok(Self { dir })
        }

        /// The portable fallback has no link-count query; every target takes
        /// the ordinary temp+rename path.
        pub(super) fn target_info(&self, name: &std::ffi::OsStr) -> io::Result<super::TargetInfo> {
            let metadata = std::fs::metadata(self.dir.join(name))?;
            Ok(super::TargetInfo {
                permissions: Some(metadata.permissions()),
                hard_links: 1,
            })
        }

        pub(super) fn open_file(
            &self,
            name: &std::ffi::OsStr,
            _nofollow: bool,
        ) -> io::Result<std::fs::File> {
            std::fs::File::open(self.dir.join(name))
        }

        pub(super) fn open_write(&self, name: &std::ffi::OsStr) -> io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .write(true)
                .open(self.dir.join(name))
        }

        pub(super) fn create(
            &self,
            name: &std::ffi::OsStr,
            _permissions: Option<&std::fs::Permissions>,
        ) -> io::Result<std::fs::File> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            options.open(self.dir.join(name))
        }

        pub(super) fn set_permissions(
            &self,
            name: &std::ffi::OsStr,
            permissions: std::fs::Permissions,
        ) {
            let _ = std::fs::set_permissions(self.dir.join(name), permissions);
        }

        pub(super) async fn replace(
            &self,
            from: &std::ffi::OsStr,
            to: &std::ffi::OsStr,
        ) -> io::Result<()> {
            tokio::fs::rename(self.dir.join(from), self.dir.join(to)).await
        }

        pub(super) fn remove(&self, name: &std::ffi::OsStr) {
            let _ = std::fs::remove_file(self.dir.join(name));
        }

        pub(super) fn sync(&self) {}
    }
}
