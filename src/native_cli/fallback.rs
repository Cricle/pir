//! Transparent fallback to the system tool: exec the original argv with
//! inherited stdio and return its exit code.
//!
//! `--no-fallback` and `PIR_NATIVE_FALLBACK=0|false` disable the fallback; the
//! command then reports an unsupported usage with the system tool's error
//! code instead of running anything else.

use std::io::Write;
use std::process::Stdio;

/// Remove every `--no-fallback` occurrence and report whether the fallback is
/// disabled (either by the flag or by the environment). Everything after a
/// `--` separator is a path/operand and is left untouched.
pub(crate) fn strip_control(args: &[String]) -> (Vec<String>, bool) {
    let mut disabled = env_disables_fallback();
    let mut clean = Vec::with_capacity(args.len());
    let mut end_of_options = false;
    for arg in args {
        if end_of_options {
            clean.push(arg.clone());
        } else if arg == "--" {
            end_of_options = true;
            clean.push(arg.clone());
        } else if arg == "--no-fallback" {
            disabled = true;
        } else {
            clean.push(arg.clone());
        }
    }
    (clean, disabled)
}

fn env_disables_fallback() -> bool {
    match std::env::var("PIR_NATIVE_FALLBACK") {
        Ok(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false"),
        Err(_) => false,
    }
}

/// Exec the first candidate program that exists, forwarding `args` verbatim
/// with inherited stdio. Returns its exit code, or 127 when no candidate can
/// be spawned.
pub(crate) fn exec(candidates: &[&str], args: &[String]) -> i32 {
    for program in candidates {
        let status = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        match status {
            Ok(status) => return status.code().unwrap_or(1),
            // Not installed under this name: try the next candidate.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                eprintln!("{program}: {error}");
                return 127;
            }
        }
    }
    eprintln!(
        "pir: no system `{}` found on PATH (native mode does not support this usage)",
        candidates.join("` / `")
    );
    127
}

/// Report a usage the native implementation does not cover while the fallback
/// is disabled.
pub(crate) fn report_unsupported(command: &str) {
    eprintln!(
        "{command}: unsupported option or usage in native mode (fallback disabled by --no-fallback / PIR_NATIVE_FALLBACK)"
    );
}

/// Flush the process streams and exit (native commands own their exit code).
pub(crate) fn exit_with(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}
