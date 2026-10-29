#![cfg(windows)]
#![allow(unsafe_code)]
//! Constrained-device budget: the Windows Job Object cap is a hard limit.
//!
//! `scripts/perf/constrained_device.ps1` owns the idle/RSS/CPU measurement.
//! This test covers the other half of the gate claim: a
//! `JOB_OBJECT_LIMIT_PROCESS_MEMORY` job really does make the child fail with
//! an allocation error instead of silently running past its budget.
//!
//! It needs no pir internals: it uses the same Win32 calls as the probe
//! (`CreateJobObjectW` + `JobObjectExtendedLimitInformation` +
//! `AssignProcessToJobObject`) and drives the real binary through
//! `CARGO_BIN_EXE_pir`. The child is created `CREATE_SUSPENDED`, so the job
//! limit exists before its first instruction; there is no assignment race.
//!
//! A tiny cap (4 MiB, below any Rust runtime's startup commit) must produce a
//! non-zero exit.  The 256/512/1024 MiB device tiers must still let
//! `pir --version` succeed, so the test also proves the limit is not just
//! "always kill the child".

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A job handle that closes (and, with KILL_ON_JOB_CLOSE, reaps) on drop.
struct Job(HANDLE);

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle was created by CreateJobObjectW and is owned by
        // this guard; closing it twice cannot happen.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn create_limited_job(cap_bytes: usize) -> Job {
    // SAFETY: plain Win32 calls with a zeroed, correctly sized information
    // struct. The SECURITY_ATTRIBUTES pointer is NULL (inherit the default).
    unsafe {
        let job = CreateJobObjectW(null(), null());
        assert_ne!(job, 0, "CreateJobObjectW failed");
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = cap_bytes;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        assert_ne!(set, 0, "SetInformationJobObject failed");

        // Read it back: a silently ignored limit would make the rest of the
        // test meaningless.
        let mut observed: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let query = QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut observed as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *mut c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        );
        assert_ne!(query, 0, "QueryInformationJobObject failed");
        assert_ne!(
            observed.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            0,
            "JOB_OBJECT_LIMIT_PROCESS_MEMORY is not set"
        );
        assert_eq!(
            observed.ProcessMemoryLimit, cap_bytes,
            "ProcessMemoryLimit did not round-trip"
        );
        Job(job)
    }
}

/// Start `pir` with `args` suspended under `job`, then resume it and return
/// its exit code once it finishes (30 s budget).
fn run_pir_suspended(job: &Job, args: &[&str]) -> u32 {
    let exe = env!("CARGO_BIN_EXE_pir");
    let exe_wide = wide(exe);
    let mut command = String::from("\"");
    command.push_str(exe);
    command.push('"');
    for arg in args {
        command.push(' ');
        command.push_str(&format!("\"{arg}\""));
    }
    let mut command_wide = wide(&command);

    // SAFETY: the command line and startup info live until CreateProcessW
    // returns; the returned handles are closed below / on drop.
    unsafe {
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            exe_wide.as_ptr(),
            command_wide.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_SUSPENDED | CREATE_NO_WINDOW,
            null(),
            null(),
            &startup,
            &mut process,
        );
        assert_ne!(created, 0, "CreateProcessW failed for {exe}");
        let assigned = AssignProcessToJobObject(job.0, process.hProcess);
        if assigned == 0 {
            let _ = CloseHandle(process.hProcess);
            let _ = CloseHandle(process.hThread);
            panic!("AssignProcessToJobObject failed (nested jobs need Windows 8+)");
        }
        let resumed = ResumeThread(process.hThread);
        assert_ne!(resumed, u32::MAX, "ResumeThread failed");
        let _ = CloseHandle(process.hThread);

        let waited = WaitForSingleObject(process.hProcess, 30_000);
        assert_eq!(waited, 0, "child did not finish within 30 s");
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(process.hProcess, &mut code);
        let _ = CloseHandle(process.hProcess);
        assert_ne!(got, 0, "GetExitCodeProcess failed");
        code
    }
}

#[test]
fn process_memory_job_limit_is_a_hard_cap() {
    // 4 MiB is below the startup commit of any working Rust binary; the child
    // must fail to allocate, not run to a clean `--version` exit.
    let job = create_limited_job(4 * 1024 * 1024);
    let code = run_pir_suspended(&job, &["--version"]);
    assert_ne!(
        code, 0,
        "pir exited 0 under a 4 MiB JOB_OBJECT_LIMIT_PROCESS_MEMORY cap; \
         the cap is not being enforced"
    );
}

#[test]
fn process_memory_job_limit_does_not_block_a_constrained_but_valid_run() {
    // The STATUS constrained-device row quotes 256/512/1024 MiB.  All three
    // device tiers must let a trivial `--version` run complete; this also
    // proves the limit is not "always kill the child".
    for cap_mib in [256usize, 512, 1024] {
        let job = create_limited_job(cap_mib * 1024 * 1024);
        let code = run_pir_suspended(&job, &["--version"]);
        assert_eq!(
            code, 0,
            "pir --version failed under a {cap_mib} MiB job cap"
        );
    }
}
