//! Cross-platform process identity probes, shared by the background-job
//! supervisor and the session lock.
//!
//! These live outside `jobs` so the core session lock keeps working when the
//! `jobs` feature is off: a recycled pid must never be signalled, and that
//! guarantee is not a job-specific concern.

/// Platform-specific process start token used to detect pid reuse.
///
/// Every caller pairs this with a liveness probe: a recorded pid only belongs
/// to this job when the process is alive AND its start token still matches, so
/// a recycled pid can never be signalled.
///
/// Linux (and any Unix with procfs): field 22 (`starttime`) of
/// `/proc/<pid>/stat`, in clock ticks since boot. macOS: the start time
/// `libproc` reports for the process (the `macOS start token` arm below) — the
/// same identity, obtained from the OS's own process-inspection API because
/// there is no `/proc`. Windows: the creation time from `GetProcessTimes`, as a
/// 100 ns FILETIME value.
///
/// Those are the only three answers that exist: `std` exposes no portable
/// "when did this pid start" query, so every branch is OS-specific by
/// necessity rather than by preference. A platform with neither procfs nor a
/// probe returns `None`, and the caller keeps the legacy pid-only behaviour —
/// a documented narrowing of assurance, not a silently skipped check.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn process_start_token(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field (2) may contain spaces and parentheses, so split after
    // its last ')'. The remainder starts at field 3 (state); `starttime` is
    // field 22, i.e. index 19 in that remainder.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// macOS start token: `(start_seconds, start_microseconds)` packed into one
/// `u64`, as `proc_pidinfo(PROC_PIDTBSDINFO)` reports them.
///
/// macOS has no procfs, so `libproc`'s `proc_pidinfo` is the supported way to
/// ask the kernel about a process. `pbi_start_tvsec`/`pbi_start_tvusec` are
/// stamped at exec and never repeat across pid reuse, which is exactly the
/// role Linux's `starttime` plays — this closes the macOS gap where a recycled
/// pid made `status`/`kill`/lock reclaim address an unrelated process
/// (JOBS-1).
///
/// The output buffer is zero-initialized rather than built field by field:
/// the kernel fills every field it knows about, and a struct literal here
/// would break the macOS build the moment libc adds a field to `proc_bsdinfo`
/// (a change this host cannot compile-check). A short read — what a vanished
/// pid returns — yields `None`, which the caller reads as "identity
/// unavailable", never as "process gone".
#[cfg(target_os = "macos")]
pub(crate) fn process_start_token(pid: u32) -> Option<u64> {
    #![allow(unsafe_code)]
    use libc::{proc_bsdinfo, proc_pidinfo, PROC_PIDTBSDINFO};

    // SAFETY: `proc_bsdinfo` is a plain C struct of integers and byte arrays,
    // so all-zero is a valid value for it; `zeroed` just performs that init.
    let mut info = unsafe { std::mem::MaybeUninit::<proc_bsdinfo>::zeroed().assume_init() };
    // SAFETY: `info` is a live, correctly-typed out-parameter and the size
    // passed is exactly its size, so `proc_pidinfo` cannot write past it.
    let written = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast::<libc::c_void>(),
            std::mem::size_of::<proc_bsdinfo>() as libc::c_int,
        )
    };
    if written != std::mem::size_of::<proc_bsdinfo>() as libc::c_int {
        return None;
    }
    // Both fields are u64. Seconds fit in 32 bits until 2106 and microseconds
    // are below 1e6, so packing them as `sec << 32 | usec` is lossless here;
    // the packing exists only to keep one token type across platforms.
    if info.pbi_start_tvsec > u64::from(u32::MAX) {
        return None;
    }
    Some((info.pbi_start_tvsec << 32) | (info.pbi_start_tvusec & 0xFFFF_FFFF))
}

#[cfg(windows)]
pub(crate) fn process_start_token(pid: u32) -> Option<u64> {
    #![allow(unsafe_code)]
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain pid; the returned handle is checked and closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return None;
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: `handle` was opened above and the four FILETIME locals are live
    // out-parameters.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: closing the handle opened above.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    Some((creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn process_start_token(_pid: u32) -> Option<u64> {
    None
}
