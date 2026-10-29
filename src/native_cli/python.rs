//! `pir python`: the embedded RustPython 3 interpreter.
//!
//! Native forms: `-c CMD`, a script path followed by its arguments, and code
//! read from stdin (no arguments, or `-`). The script's path becomes
//! `sys.argv[0]`, `__name__` is `__main__`, and a script's directory is
//! prepended to `sys.path`, matching CPython. Anything else (`-m`, `-u`,
//! `-X`, `-W`, `-O`, `-I`, `-S`, `-B`, `-E`, `-V`, `-h`, `-i`, …) falls back
//! to the system `python3`/`python`.
//!
//! stdout and stderr are written straight to the process streams; the exit
//! code is 0 on success and 1 when the script raised.

use super::RunOutcome;

#[cfg(feature = "python")]
use std::io::Write;

#[cfg(feature = "python")]
use std::time::Duration;

#[cfg(feature = "python")]
use crate::plugins::builtin::python_vm::{failed_outcome, PythonJob, PythonSession};

/// No wall-clock deadline for a CLI run (the embedded session's watchdog only
/// fires at this bound; a day is effectively "none").
#[cfg(feature = "python")]
const CLI_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[cfg(feature = "python")]
pub(crate) fn run(args: &[String]) -> RunOutcome {
    let (source, argv, script_dir) = match parse(args) {
        Ok(parsed) => parsed,
        Err(outcome) => return outcome,
    };
    let code = build_source(&source, &argv, script_dir.as_deref());
    let outcome = execute(code);

    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(outcome.stdout.as_bytes());
    let _ = stdout.flush();
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(outcome.stderr.as_bytes());
    let _ = stderr.flush();

    RunOutcome::Exit(if outcome.error { 1 } else { 0 })
}

#[cfg(not(feature = "python"))]
pub(crate) fn run(_args: &[String]) -> RunOutcome {
    // The interpreter rides the `python` feature; without it the command is
    // the system python.
    RunOutcome::Unsupported
}

/// Resolve the code, `sys.argv`, and (for script files) the directory to add
/// to `sys.path`.
#[cfg(feature = "python")]
fn parse(args: &[String]) -> Result<(String, Vec<String>, Option<String>), RunOutcome> {
    match args.first().map(String::as_str) {
        // No arguments: read the program from stdin, `sys.argv[0] == ""`.
        None => {
            let source = read_stdin()?;
            Ok((source, vec![String::new()], None))
        }
        // `-`: stdin, `sys.argv[0] == "-"`.
        Some("-") => {
            let source = read_stdin()?;
            let mut argv = vec!["-".to_owned()];
            argv.extend(args[1..].iter().cloned());
            Ok((source, argv, None))
        }
        Some("-c") => {
            let Some(source) = args.get(1).cloned() else {
                // CPython reports a usage error; let the system tool do it.
                return Err(RunOutcome::Unsupported);
            };
            let mut argv = vec!["-c".to_owned()];
            argv.extend(args[2..].iter().cloned());
            Ok((source, argv, None))
        }
        // Any other option (-m, -u, -X, -W, -O, -I, -S, -B, -E, -V, -h, -i).
        Some(first) if first.starts_with('-') => Err(RunOutcome::Unsupported),
        Some(script) => {
            let bytes = match std::fs::read(script) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("python: can't open file '{script}': {}", io_message(&error));
                    return Err(RunOutcome::Exit(2));
                }
            };
            let source = match String::from_utf8(bytes) {
                Ok(source) => source,
                // Binary/legacy-encoded scripts: the system interpreter owns
                // the encoding declaration.
                Err(_) => return Err(RunOutcome::Unsupported),
            };
            let source = source
                .strip_prefix('\u{feff}')
                .map(str::to_owned)
                .unwrap_or(source);
            let mut argv = vec![script.to_owned()];
            argv.extend(args[1..].iter().cloned());
            let directory = std::path::Path::new(script)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| parent.to_string_lossy().into_owned());
            Ok((source, argv, directory))
        }
    }
}

#[cfg(feature = "python")]
fn read_stdin() -> Result<String, RunOutcome> {
    use std::io::Read;
    let mut bytes = Vec::new();
    if std::io::stdin().read_to_end(&mut bytes).is_err() {
        return Err(RunOutcome::Unsupported);
    }
    String::from_utf8(bytes).map_err(|_| RunOutcome::Unsupported)
}

/// Prepend the CPython module preamble (argv, `__name__`, script directory).
/// It is a single physical line so traceback line numbers shift by exactly
/// one.
#[cfg(feature = "python")]
fn build_source(source: &str, argv: &[String], script_dir: Option<&str>) -> String {
    let mut preamble = String::from("import sys as __pir_sys__; __pir_sys__.argv = [");
    for (index, argument) in argv.iter().enumerate() {
        if index > 0 {
            preamble.push_str(", ");
        }
        preamble.push_str(&python_literal(argument));
    }
    preamble.push(']');
    if let Some(directory) = script_dir {
        preamble.push_str("; __pir_sys__.path.insert(0, ");
        preamble.push_str(&python_literal(directory));
        preamble.push(')');
    }
    preamble.push_str("; del __pir_sys__; __name__ = '__main__'\n");

    let mut code = preamble;
    code.push_str(source);
    if !source.ends_with('\n') {
        code.push('\n');
    }
    code
}

#[cfg(feature = "python")]
fn python_literal(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('\'');
    for character in text.chars() {
        match character {
            '\\' => literal.push_str("\\\\"),
            '\'' => literal.push_str("\\'"),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            character => literal.push(character),
        }
    }
    literal.push('\'');
    literal
}

/// Run the code on a dedicated thread with its own runtime: the command is
/// dispatched from inside the host's async runtime, and the embedded session
/// must not be driven from a `block_on` nested in it.
#[cfg(feature = "python")]
fn execute(code: String) -> crate::plugins::builtin::python_vm::ExecOutcome {
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let worker = std::thread::Builder::new()
        .name("pir-native-python".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return failed_outcome(&format!("python: {error}")),
            };
            runtime.block_on(async move {
                let session = PythonSession::new();
                let job = PythonJob {
                    code,
                    cwd,
                    // No REPL-style repr echo: CLI output is explicit stdout.
                    eval_first: false,
                    #[cfg(feature = "context-mode")]
                    globals: Vec::new(),
                };
                session.exec(job, CLI_TIMEOUT).await
            })
        });
    match worker {
        Ok(handle) => handle
            .join()
            .unwrap_or_else(|_| failed_outcome("python: the interpreter thread panicked")),
        Err(error) => failed_outcome(&format!("python: {error}")),
    }
}

#[cfg(feature = "python")]
fn io_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "[Errno 2] No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "[Errno 13] Permission denied".to_owned(),
        _ => error.to_string(),
    }
}
