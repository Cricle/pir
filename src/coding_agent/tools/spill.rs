//! Restricted spill files for the shell tools' bounded output capture.
//!
//! The bash/powershell tools spill an evicted output prefix to a temp file and
//! advertise its path as `fullOutputPath` / `full_output_path`; the model reads
//! it with a follow-up tool call, so the file has to outlive the run that
//! produced it. Two properties matter for that file:
//!
//! - **Confidentiality.** Command output routinely contains secrets. On Unix
//!   the file is created with mode 0600, so there is never a window in which
//!   another user could open it. On Windows it is created through
//!   `SECURITY_ATTRIBUTES` carrying a DACL that grants the current user
//!   `FILE_ALL_ACCESS` and nobody else — applying the DACL at creation time
//!   (rather than patching it afterwards) keeps the file from existing for
//!   even a moment under the inherited directory ACL.
//! - **Bounded lifetime.** An *advertised* file is kept for the follow-up read,
//!   but not forever: every creation (i.e. at tool start, including the first
//!   truncated command after a process start) sweeps files older than
//!   [`SPILL_MAX_AGE`], trims the set down to [`SPILL_MAX_FILES`] and enforces
//!   a [`SPILL_MAX_BYTES`] total-bytes cap, oldest first. A capture that never
//!   advertises its file (the run aborted, or the file turned out incomplete)
//!   deletes it through its own RAII guard instead of leaving garbage behind.
//!
//! The name shape (`pir-shell-{pid}-{seq}-{uuid-v4}`) is load-bearing: it is
//! what the security regression test pins, what makes a pre-planted name
//! unguessable in the shared temp directory, and what the sweep recognizes as
//! its own.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Prefix of every spill file this module creates and may sweep.
pub const SPILL_PREFIX: &str = "pir-shell-";

/// How long an advertised spill file stays available for the model's
/// follow-up read.
const SPILL_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How many spill files one temp directory may hold before the sweep drops the
/// oldest. Bounds disk use when a long session truncates many commands.
const SPILL_MAX_FILES: usize = 32;

/// Total bytes the retained spill files may occupy before the sweep starts
/// dropping the oldest. A single command can spill an arbitrarily large
/// stream, so the file-count cap alone does not bound disk use.
const SPILL_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Hard cap on ONE spill file. The total-bytes sweep bounds the directory
/// over time, but a single command can emit an unbounded stream between
/// sweeps; the writer refuses to grow a file past this bound (the capture
/// then latches its spill failure and reports the in-memory tail as
/// truncated instead of advertising an incomplete file).
pub const SPILL_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Minimum spacing between sweeps: the sweep walks the whole temp directory,
/// and spill creation sits on the output path, so it must not run per file.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Process-unique sequence for spill temp-file names: combined with a
/// fixed-length random suffix it keeps concurrent runs from colliding and
/// makes pre-planted names unguessable in the shared temp directory.
static SPILL_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// The throttle for [`sweep_spills`] (`None` until the first sweep).
static LAST_SWEEP: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// One restricted spill file with a cumulative per-file byte cap.
///
/// All writes go through [`SpillFile::write_all`], which refuses to grow the
/// file past [`SPILL_MAX_FILE_BYTES`]. On refusal the caller must treat the
/// file as incomplete (it holds only a prefix of the stream) and must not
/// advertise its path as the full output.
pub struct SpillFile {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
    limit: u64,
}

impl SpillFile {
    /// Append `bytes`, enforcing the per-file cap. The write is all-or-nothing
    /// with respect to the cap: a chunk that would cross it is not written at
    /// all, so the file stays a valid prefix of the stream.
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let total = self.written.saturating_add(bytes.len() as u64);
        if total > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "spill file would exceed the {}-byte per-file cap",
                    self.limit
                ),
            ));
        }
        self.file.write_all(bytes)?;
        self.written = total;
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written so far (the cap's basis).
    pub fn written(&self) -> u64 {
        self.written
    }
}

/// Create a spill temp file under a fresh name
/// `pir-shell-{pid}-{seq}-{uuid-v4}`, opened exclusively (`create_new`, the
/// Rust O_EXCL): the call fails rather than following a symlink or overwriting
/// a pre-planted file in the shared temp directory. The file is owner-only
/// from the moment it exists (see the module docs). `None` when creation fails
/// (the capture then degrades to the in-memory tail).
///
/// The returned [`SpillFile`] is what the tools advertise; callers are
/// responsible for handing its path out, or for deleting the file when it
/// never gets advertised.
pub fn create_spill_file() -> Option<SpillFile> {
    create_spill_file_with_limit(SPILL_MAX_FILE_BYTES)
}

/// [`create_spill_file`] with an explicit per-file cap. Production always
/// uses [`SPILL_MAX_FILE_BYTES`]; this seam lets the cap regression test use a
/// tiny file instead of writing 64 MiB.
#[doc(hidden)]
pub fn create_spill_file_with_limit(limit: u64) -> Option<SpillFile> {
    sweep_spills();
    let path = std::env::temp_dir().join(format!(
        "{SPILL_PREFIX}{}-{}-{}",
        std::process::id(),
        SPILL_FILE_SEQ.fetch_add(1, Ordering::Relaxed),
        uuid::Uuid::new_v4().simple()
    ));
    let file = platform::create_restricted(&path)?;
    Some(SpillFile {
        file,
        path,
        written: 0,
        limit,
    })
}

/// Force one sweep now, bypassing [`SWEEP_INTERVAL`]. Production calls
/// [`create_spill_file`] (which sweeps on its own throttle); this seam exists
/// so the security regression tests can pin the age/count/bytes policy
/// deterministically.
#[doc(hidden)]
pub fn sweep_spills_now() {
    sweep_spills_inner(true);
}

/// Sweep expired and excess spill files (throttled to [`SWEEP_INTERVAL`]).
/// Best effort by design: a temp directory that cannot be listed or a file
/// that cannot be removed never fails the run that triggered the sweep.
fn sweep_spills() {
    sweep_spills_inner(false);
}

/// The pid embedded in a `pir-shell-<pid>-<seq>-<uuid>` file name (the same
/// name shape [`create_spill_file`] writes; a name without a numeric pid — a
/// foreign or pre-versioning artifact — is never reclaimed).
fn spill_file_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(SPILL_PREFIX)?;
    let (pid, rest) = rest.split_once('-')?;
    if pid.is_empty() || rest.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Whether `pid` still names a live process.
///
/// Conservative on every unknown (an unreadable process reports alive), so a
/// sweep can never delete a running session's advertised spill file. This is
/// the cross-process guarantee: another live pir (or a pid we cannot probe)
/// keeps its files; only provably dead pids and this process are swept.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Linux exposes a cheap safe probe. Other Unix platforms have no safe
    // liveness API without a new `unsafe` boundary, so they answer
    // conservatively: expired foreign files are left in place.
    let proc_root = std::path::Path::new("/proc");
    if proc_root.is_dir() {
        return proc_root.join(pid.to_string()).exists();
    }
    true
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    // `%SystemRoot%\System32\tasklist.exe` by absolute path (never a bare name
    // resolved against the current directory or PATH), plus
    // `NoDefaultCurrentDirectoryInExePath` for the helper's own spawns.
    // CREATE_NO_WINDOW keeps it from flashing a console. Any failure is
    // treated as "alive" (conservative).
    let system_root = std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));
    let output = std::process::Command::new(
        std::path::Path::new(&system_root)
            .join("System32")
            .join("tasklist.exe"),
    )
    .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
    .env("NoDefaultCurrentDirectoryInExePath", "1")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .creation_flags(0x0800_0000)
    .output();
    let Ok(output) = output else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let wanted = pid.to_string();
    stdout.lines().any(|line| {
        line.split(',')
            .nth(1)
            .map(|field| field.trim().trim_matches('"'))
            == Some(wanted.as_str())
    })
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn sweep_spills_inner(force: bool) {
    let throttle = LAST_SWEEP.get_or_init(|| Mutex::new(None));
    {
        let Ok(mut last) = throttle.lock() else {
            return;
        };
        if !force {
            match *last {
                Some(at) if at.elapsed() < SWEEP_INTERVAL => return,
                _ => {}
            }
        }
        *last = Some(Instant::now());
    }
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = SystemTime::now();
    let own_pid = std::process::id();
    // Only THIS process's files count toward the count/byte caps. A
    // concurrent pir session has its own budget; a global cap made one busy
    // session delete another live session's advertised spill.
    let mut own: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(SPILL_PREFIX) {
            continue;
        }
        // The owning pid is part of the name; a name we cannot attribute is
        // never deleted (the old sweep globbed every `pir-shell-*`).
        let Some(pid) = spill_file_pid(name) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let expired = now
            .duration_since(modified)
            .map(|age| age > SPILL_MAX_AGE)
            .unwrap_or(false);
        if expired {
            // Age alone is not ownership: reclaim only our own files or a
            // provably dead process's, so a long-running foreign session keeps
            // the file it still advertises.
            if pid == own_pid || !process_is_alive(pid) {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        if pid == own_pid {
            own.push((modified, entry.path(), metadata.len()));
        }
    }
    // Oldest first: the excess (by count or by bytes) is the front of the
    // list. Both caps are enforced because one huge spill must not survive
    // just because it is one of few files.
    own.sort_by_key(|(modified, _, _)| *modified);
    let mut remove = own.len().saturating_sub(SPILL_MAX_FILES);
    let mut total_bytes: u64 = own.iter().skip(remove).map(|(_, _, size)| *size).sum();
    while total_bytes > SPILL_MAX_BYTES && remove < own.len() {
        total_bytes = total_bytes.saturating_sub(own[remove].2);
        remove += 1;
    }
    for (_, path, _) in own.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(unix)]
mod platform {
    use std::path::Path;

    /// Owner-only mode applied at creation (upstream's temp-file mode): the
    /// mode is part of the `open`, so the spill never exists with group/other
    /// bits set, not even briefly.
    pub(super) fn create_restricted(path: &Path) -> Option<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options.open(path).ok()
    }
}

#[cfg(windows)]
mod platform {
    //! `SECURITY_ATTRIBUTES` with a current-user DACL, applied by `CreateFileW`
    //! at creation time.
    //!
    //! Two deliberate choices:
    //! * the DACL is marked `SE_DACL_PROTECTED`, so inheritable ACEs from the
    //!   temp directory (which on some hosts grant `Users`/`Authenticated
    //!   Users` access) are never merged into the file's DACL. An
    //!   administrator can still take ownership and rewrite it — the goal is
    //!   "no other unprivileged user", not "unrecoverable";
    //! * any failure degrades to a plain `create_new` (the temp directory's own
    //!   per-user ACL is the floor) rather than failing the run: the spill file
    //!   is a convenience artifact, and a DACL the object system rejects must
    //!   not turn a completed command into an error.

    // The safe `std::fs` API cannot carry a security descriptor, so this module
    // is an `unsafe` boundary, exactly like `src/env_daemon/sys/windows.rs`.
    #![allow(unsafe_code)]

    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_TEMPORARY, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Create the file with a DACL that allows only the current user.
    pub(super) fn create_restricted(path: &Path) -> Option<std::fs::File> {
        if let Some(file) = create_with_restricted_dacl(path) {
            return Some(file);
        }
        tracing::debug!(
            target: "pir::tools::spill",
            path = %path.display(),
            "spill file: restricted DACL unavailable; falling back to the directory ACL"
        );
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        options.open(path).ok()
    }

    fn create_with_restricted_dacl(path: &Path) -> Option<std::fs::File> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let sid = Sid::current_user()?;
        let mut acl = Acl::for_sid(&sid)?;
        let descriptor = SecurityDescriptor::with_dacl(&mut acl)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.pointer,
            bInheritHandle: 0,
        };
        // SAFETY: `wide` is NUL-terminated and outlives the call; `attributes`
        // points at a live self-relative-free SECURITY_DESCRIPTOR whose DACL
        // (`acl`) outlives the call as well.
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_WRITE_DATA,
                // DELETE sharing keeps the RAII guard's `remove_file` working
                // while the write handle is still owned by the guard; the DACL
                // (not the share mode) is what denies other users.
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_TEMPORARY,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: `CreateFileW` just returned a valid handle that this function
        // uniquely owns (nothing else holds it, so no double close).
        Some(unsafe { std::fs::File::from_raw_handle(handle as _) })
    }

    /// The current process token's user SID, in 8-byte-aligned storage (the
    /// ACL entry points require a 4-byte-aligned SID).
    struct Sid {
        storage: Vec<u64>,
        offset: usize,
        length: usize,
    }

    impl Sid {
        fn current_user() -> Option<Self> {
            // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is
            // always valid and must not be closed.
            let process = unsafe { GetCurrentProcess() };
            let mut token: HANDLE = 0;
            // SAFETY: `token` is a live out-parameter; TOKEN_QUERY is the
            // minimal access the lookup needs.
            let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
            if opened == 0 {
                return None;
            }
            let sid = Self::read(token);
            // SAFETY: the handle was opened above and is still owned here.
            unsafe {
                CloseHandle(token);
            }
            sid
        }

        fn read(token: HANDLE) -> Option<Self> {
            let mut needed: u32 = 0;
            // SAFETY: a null buffer with length 0 asks only for the required
            // size; the documented failure still fills `needed`.
            unsafe {
                GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            }
            if needed == 0 {
                return None;
            }
            let mut storage = vec![0u64; (needed as usize).div_ceil(std::mem::size_of::<u64>())];
            // SAFETY: the buffer is at least `needed` bytes and 8-byte aligned.
            let filled = unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    storage.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            };
            if filled == 0 {
                return None;
            }
            // SAFETY: the call succeeded, so the buffer starts with a
            // TOKEN_USER (no pointer fix-up needed: TOKEN_USER lives at the
            // buffer base and its SID is inside the same allocation).
            let user = unsafe { &*(storage.as_ptr().cast::<TOKEN_USER>()) };
            // SAFETY: `Sid` points into the buffer this call just filled.
            let length = unsafe { GetLengthSid(user.User.Sid) } as usize;
            if length == 0 || length > needed as usize {
                return None;
            }
            // SAFETY: the SID occupies `length` bytes inside `storage`.
            let offset =
                unsafe { (user.User.Sid as *const u8).offset_from(storage.as_ptr().cast()) };
            let start = usize::try_from(offset).ok()?;
            if start + length > storage.len() * std::mem::size_of::<u64>() {
                return None;
            }
            Some(Self {
                storage,
                offset: start,
                length,
            })
        }

        fn pointer(&self) -> *mut core::ffi::c_void {
            // The SID starts `offset` bytes into the token buffer, after the
            // `TOKEN_USER` header; not at the buffer base.
            self.storage.as_ptr().cast::<u8>().wrapping_add(self.offset) as *mut core::ffi::c_void
        }
    }

    /// An `ACL` with one allow-ACE for the current user, in aligned storage.
    struct Acl {
        storage: Vec<u64>,
        pointer: *mut ACL,
        size: u32,
    }

    impl Acl {
        fn for_sid(sid: &Sid) -> Option<Self> {
            // `ACL` header + one ACCESS_ALLOWED_ACE (whose `SidStart` field
            // overlaps the SID's first four bytes) + the SID's remaining bytes.
            let size = std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                - std::mem::size_of::<u32>()
                + sid.length;
            let storage = vec![0u64; size.div_ceil(std::mem::size_of::<u64>())];
            let mut acl = Self {
                storage,
                pointer: std::ptr::null_mut(),
                size: size as u32,
            };
            acl.pointer = acl.storage.as_mut_ptr().cast();
            // SAFETY: the buffer is 8-byte aligned and at least `size` bytes;
            // `ACL_REVISION` is the required revision for a plain file ACL.
            if unsafe { InitializeAcl(acl.pointer, acl.size, ACL_REVISION) } == 0 {
                return None;
            }
            // `AddAccessAllowedAce` copies the SID into the ACL, so `sid` only
            // has to stay alive for the duration of this call.
            //
            // SAFETY: `acl.pointer` points into `acl.storage` (which `acl`
            // keeps alive), the ACE fits (the size above reserves it), and
            // `sid.pointer()` is a valid, aligned SID.
            if unsafe {
                AddAccessAllowedAce(acl.pointer, ACL_REVISION, FILE_ALL_ACCESS, sid.pointer())
            } == 0
            {
                return None;
            }
            Some(acl)
        }
    }

    struct SecurityDescriptor {
        #[allow(
            dead_code,
            reason = "the box owns the memory the descriptor pointer refers to"
        )]
        storage: Box<SECURITY_DESCRIPTOR>,
        pointer: *mut core::ffi::c_void,
    }

    impl SecurityDescriptor {
        fn with_dacl(acl: &mut Acl) -> Option<Self> {
            // SAFETY: `SECURITY_DESCRIPTOR` is plain old data; all-zero is a
            // valid value and `InitializeSecurityDescriptor` initializes it
            // before any read.
            let mut storage: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
            let pointer: *mut core::ffi::c_void =
                (&mut *storage as *mut SECURITY_DESCRIPTOR).cast();
            // SAFETY: `storage` is a live, aligned SECURITY_DESCRIPTOR; revision
            // 1 is `SECURITY_DESCRIPTOR_REVISION`.
            if unsafe { InitializeSecurityDescriptor(pointer, 1) } == 0 {
                return None;
            }
            // SAFETY: `acl.pointer` points into `acl.storage`, which outlives
            // this descriptor (both are locals of the caller's scope that use
            // them together); the descriptor only stores the pointer.
            if unsafe { SetSecurityDescriptorDacl(pointer, 1, acl.pointer, 0) } == 0 {
                return None;
            }
            // SAFETY: `storage` is a live, absolute SECURITY_DESCRIPTOR whose
            // DACL was just set; the control bits being changed exist on it.
            if unsafe {
                SetSecurityDescriptorControl(pointer, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
            } == 0
            {
                return None;
            }
            Some(Self { storage, pointer })
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::path::Path;

    pub(super) fn create_restricted(path: &Path) -> Option<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        options.open(path).ok()
    }
}
