//! `pir grep` / `pir sed` / `pir python` — the standalone native command
//! surface (no agent, session, model or config load).
//!
//! Each command forwards its argv verbatim. The native implementation handles
//! the common cases in-process (the grep tool's ripgrep engine, a sed subset,
//! the embedded RustPython interpreter); anything it does not support is
//! transparently exec'd against the system `grep`/`sed`/`python3` with the
//! original argv, so the commands are drop-in for scripts that call those
//! tools. `--no-fallback` (or `PIR_NATIVE_FALLBACK=0`) disables the fallback.
//!
//! The bash and powershell tools put `PIR_BIN` on the child environment and
//! prepend the binary's directory to the child `PATH`, so a model running
//! inside the agent reaches these commands as plain `pir grep …`.
//!
//! Exit codes follow the system tools: grep 0/1/2 (match / no match / error),
//! sed 0/1/2 (success / script error / file error), python 0/1/2.

mod fallback;
mod grep;
mod python;
mod sed;

/// What a native command implementation decided.
pub(crate) enum RunOutcome {
    /// The command ran natively and produced this process exit code.
    Exit(i32),
    /// The native implementation does not cover this argv: fall back to the
    /// system tool (or fail when the fallback is disabled).
    Unsupported,
}

/// Run `pir grep <args…>`.
pub fn run_grep(args: &[String]) -> crate::Result<()> {
    let (args, disabled) = fallback::strip_control(&command_args("grep", args));
    match grep::run(&args) {
        RunOutcome::Exit(code) => fallback::exit_with(code),
        RunOutcome::Unsupported => {
            if disabled {
                fallback::report_unsupported("grep");
                fallback::exit_with(2);
            }
            fallback::exit_with(fallback::exec(&["grep"], &args));
        }
    }
}

/// Run `pir sed <args…>`.
pub fn run_sed(args: &[String]) -> crate::Result<()> {
    let (args, disabled) = fallback::strip_control(&command_args("sed", args));
    match sed::run(&args) {
        RunOutcome::Exit(code) => fallback::exit_with(code),
        RunOutcome::Unsupported => {
            if disabled {
                fallback::report_unsupported("sed");
                fallback::exit_with(1);
            }
            fallback::exit_with(fallback::exec(&["sed"], &args));
        }
    }
}

/// Run `pir python <args…>`.
pub fn run_python(args: &[String]) -> crate::Result<()> {
    let (args, disabled) = fallback::strip_control(&command_args("python", args));
    match python::run(&args) {
        RunOutcome::Exit(code) => fallback::exit_with(code),
        RunOutcome::Unsupported => {
            if disabled {
                fallback::report_unsupported("python");
                fallback::exit_with(2);
            }
            // Windows/macOS commonly ship `python3`, some installs only
            // `python`; try both in that order (CPython's own launcher
            // convention).
            fallback::exit_with(fallback::exec(&["python3", "python"], &args));
        }
    }
}

/// The command's argv as the user typed it.
///
/// `preprocess_args` runs before clap and strips unknown `--long` options
/// (they are collected for extension `registerFlag` definitions), so the
/// clap-provided `args` can be missing exactly the options these commands
/// need (`--include`, `--color=never`, `--version`, …). For the normal shape
/// `pir <command> …` the raw argv is authoritative and is used verbatim —
/// which also makes the fallback exec the original command line. When global
/// flags precede the subcommand the clap args are used instead (the collected
/// `--no-fallback` switch is restored).
fn command_args(command: &str, clap_args: &[String]) -> Vec<String> {
    use std::ffi::OsStr;

    let mut raw = std::env::args_os();
    let _program = raw.next();
    let is_subcommand = raw
        .next()
        .is_some_and(|arg| arg.as_os_str() == OsStr::new(command));
    let mut args = if is_subcommand {
        raw.map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<String>>()
    } else {
        clap_args.to_vec()
    };
    // `preprocess_args` strips unknown `--long` options before clap runs; a
    // `--no-fallback` written before the subcommand only survives in that
    // collected map, so restore it.
    if !args.iter().any(|arg| arg == "--no-fallback")
        && crate::coding_agent::cli::collected_unknown_flags().contains_key("no-fallback")
    {
        args.push("--no-fallback".to_owned());
    }
    args
}

/// Translate a POSIX basic regular expression (BRE, grep/sed's default
/// dialect) into the `regex` crate's syntax.
///
/// In a BRE, `(`, `)`, `|`, `+`, `?`, `{`, `}` are literal characters while
/// `\(`, `\)`, `\|`, `\+`, `\?`, `\{`, `\}` are the operators — exactly the
/// opposite of the Rust regex reading. The translation swaps those two
/// groups, so `\(foo\)` becomes a capture group and a bare `foo+` stays a
/// literal plus.
///
/// `None` means "the native engine cannot reproduce this pattern": a
/// backreference (`\1`), or a bracket expression containing a backslash
/// (whose escapes are literal in a BRE but not in the Rust dialect). The
/// caller then falls back to the system tool.
pub(crate) fn bre_to_regex(pattern: &str) -> Option<String> {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.next()? {
                // BRE operators become ERE/Rust operators.
                '(' | ')' | '|' | '+' | '?' | '{' | '}' => out.push(character),
                // Backreferences are not supported by the Rust regex crate.
                '1'..='9' => return None,
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            '[' => {
                out.push('[');
                // Copy the bracket expression verbatim: its contents are
                // literal in a BRE, so a backslash cannot be translated.
                let mut first = true;
                loop {
                    let character = chars.next()?;
                    if character == '\\' {
                        return None;
                    }
                    out.push(character);
                    if character == ']' && !first {
                        break;
                    }
                    // POSIX: `]` right after `[` or `[^` is a literal member.
                    first = first && character == '^';
                }
            }
            // BRE literals that are operators in the Rust dialect.
            '(' | ')' | '|' | '+' | '?' | '{' | '}' => {
                out.push('\\');
                out.push(character);
            }
            other => out.push(other),
        }
    }
    Some(out)
}
