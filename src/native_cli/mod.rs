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
//! sed 0/1/2 (success / script error / file error), python 0/N/1/2 (success /
//! `sys.exit(N)` / uncaught exception / file error).

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
    /// The native implementation consumed `stdin` and could not handle it.
    /// The original bytes are carried so the system tool receives them
    /// through a pipe: stdin is already at EOF, so a fallback that inherits
    /// the handle would read empty input and silently discard the data.
    UnsupportedWithStdin(Vec<u8>),
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
        // Native grep never consumes stdin before falling back; handle the
        // variant defensively anyway so this match stays total and no data
        // would be dropped if that ever changes.
        RunOutcome::UnsupportedWithStdin(bytes) => {
            if disabled {
                fallback::report_unsupported("grep");
                fallback::exit_with(2);
            }
            fallback::exit_with(fallback::exec_with_stdin(&["grep"], &args, &bytes));
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
        RunOutcome::UnsupportedWithStdin(bytes) => {
            if disabled {
                fallback::report_unsupported("sed");
                fallback::exit_with(1);
            }
            fallback::exit_with(fallback::exec_with_stdin(&["sed"], &args, &bytes));
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
        RunOutcome::UnsupportedWithStdin(bytes) => {
            if disabled {
                fallback::report_unsupported("python");
                fallback::exit_with(2);
            }
            fallback::exit_with(fallback::exec_with_stdin(
                &["python3", "python"],
                &args,
                &bytes,
            ));
        }
    }
}

/// Global options that consume the following argv token. A bare `--flag`
/// listed here skips its value while [`locate_native_subcommand`] looks for
/// the subcommand; `--flag=value` is self-contained and needs no skip.
///
/// The subcommand-scoped spellings (`--unix`, `--enable`, `--disable`,
/// `--source`, `--update-extension`) cannot legally precede `grep`/`sed`/
/// `python` in a clap-accepted argv, but listing them keeps the scan honest
/// if the argument order ever loosens.
const VALUE_TAKING_GLOBAL_FLAGS: &[&str] = &[
    "extension",
    "mode",
    "session",
    "session-id",
    "fork",
    "provider",
    "model",
    "api-key",
    "system-prompt",
    "append-system-prompt",
    "thinking",
    "session-dir",
    "name",
    "models",
    "tools",
    "exclude-tools",
    "skill",
    "prompt-template",
    "theme",
    "use-theme",
    "list-models",
    "export",
    "tui-mode",
    "print",
    "unix",
    "enable",
    "disable",
    "source",
    "update-extension",
];

/// Short options (bundled upstream spellings included) that take a value.
const VALUE_TAKING_SHORT_FLAGS: &[&str] = &["e", "t", "xt", "n", "p"];

/// Find `command` in the raw argv and return every argument after it.
///
/// `preprocess_args` runs before clap and strips unknown `--long` options
/// (they are collected for extension `registerFlag` definitions), so the
/// clap-provided `args` can be missing exactly the options these commands
/// need (`--include`, `--color=never`, `--version`, …). Scanning the raw argv
/// also sees the original spelling when global flags precede the subcommand
/// (`pir --verbose grep --include=…`), where the old first-token check gave
/// up and fell back to the lossy clap args.
///
/// An *unrecognized* long flag is the one shape the raw scan cannot resolve:
/// `preprocess_args` attaches a following non-flag token as that flag's value
/// (`--ext-flag grep` → `ext-flag = "grep"`), so a token that spells the
/// command may actually be a flag value. The collected-unknown-flag map tells
/// us which flags were unknown; when one is followed by a value-shaped token
/// this returns `None` and the caller keeps the clap-provided argv instead of
/// guessing (`--ext-flag grep grep …` must not run `grep` with pattern
/// `grep`). Conservative on purpose: a wrong native argv silently searches
/// the wrong target.
///
/// Returns `None` when the shape is not confidently recognized (`--`
/// separator before the subcommand, an unknown flag value in the way, or the
/// command never appears); the caller then keeps the clap-provided argv.
fn locate_native_subcommand(command: &str) -> Option<Vec<String>> {
    let unknown_flags = crate::coding_agent::cli::collected_unknown_flags();
    let mut arguments = std::env::args_os().skip(1).peekable();
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        if text == "--" {
            // Everything after `--` is positional; no subcommand follows.
            return None;
        }
        if text == command {
            return Some(
                arguments
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            );
        }
        if let Some(long) = text.strip_prefix("--") {
            if long.contains('=') {
                // Self-contained `--flag=value`: nothing follows to skip.
                continue;
            }
            if VALUE_TAKING_GLOBAL_FLAGS.contains(&long) {
                let _ = arguments.next();
                continue;
            }
            // `preprocess_args` has already run in the clap-dispatched path
            // (the raw-first-token path returns at the first argument), so
            // its collected map is authoritative for "this flag is unknown".
            if unknown_flags.contains_key(long) {
                let next_is_value = arguments.peek().is_some_and(|next| {
                    let next = next.to_string_lossy();
                    !next.starts_with('-') && !next.starts_with('@')
                });
                if next_is_value {
                    return None;
                }
            }
        } else if let Some(short) = text.strip_prefix('-') {
            if VALUE_TAKING_SHORT_FLAGS.contains(&short) {
                let _ = arguments.next();
            }
        }
    }
    None
}

/// The command's argv as the user typed it.
fn command_args(command: &str, clap_args: &[String]) -> Vec<String> {
    let mut args = match locate_native_subcommand(command) {
        Some(raw) => raw,
        // Not confidently locatable: keep the historical clap-provided argv.
        None => clap_args.to_vec(),
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
