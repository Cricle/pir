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
//! code is 0 on success, N for `sys.exit(N)`, 1 for an uncaught exception or
//! a non-integer `sys.exit` argument, and 2 when the script file cannot be
//! opened.

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
    // CPython's top-level `-h`/`-V` never fall through to a system
    // interpreter. On a Windows host without `python3.exe` that fallback was a
    // silent rc 9009 (A27-F08).
    if let Some(first) = args.first().map(String::as_str) {
        match first {
            "-h" | "--help" => {
                println!(
                    "usage: pir python [option] ... [-c cmd | - | file | -] [arg] ...\n\
                     \n\
                     Embedded RustPython 3.12 interpreter.\n\
                     Options:\n\
                       -c cmd      program passed in as string\n\
                       -           program read from stdin\n\
                       -h, --help  show this help and exit\n\
                       -V, --version  show the Python version and exit"
                );
                return RunOutcome::Exit(0);
            }
            "-V" | "--version" => {
                println!("Python 3.12.0");
                return RunOutcome::Exit(0);
            }
            _ => {}
        }
    }
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

    RunOutcome::Exit(
        outcome
            .exit_code
            .unwrap_or(if outcome.error { 1 } else { 0 }),
    )
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
    // A non-UTF-8 program is not something the embedded interpreter can run;
    // hand the original bytes to the system interpreter through a pipe (the
    // stdin handle itself is already at EOF).
    String::from_utf8(bytes).map_err(|error| RunOutcome::UnsupportedWithStdin(error.into_bytes()))
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

    // CPython requires `from __future__ import …` to be the first statement
    // after the module docstring. A preamble before it makes the embedded
    // compiler reject the script (`InvalidFuturePlacement`), so insert after
    // that prefix when it is recognized; otherwise keep the historical
    // prepend (offset 0).
    let offset = future_import_offset(source);
    let mut code = String::with_capacity(preamble.len() + source.len() + 1);
    if offset == 0 || !source.is_char_boundary(offset) {
        code.push_str(&preamble);
        code.push_str(source);
    } else {
        let (head, tail) = source.split_at(offset);
        code.push_str(head);
        if !head.ends_with('\n') {
            code.push('\n');
        }
        code.push_str(&preamble);
        code.push_str(tail);
    }
    if !source.ends_with('\n') {
        code.push('\n');
    }
    code
}

/// Byte offset where the preamble can be inserted without breaking
/// `from __future__` placement: just past a leading module docstring and the
/// consecutive `from __future__ import …` statements that follow it.
///
/// Returns 0 when no future import is recognized (the caller then prepends
/// the preamble exactly as before). The scan is deliberately conservative:
/// any leading shape it cannot parse (unterminated string, trailing code
/// after the docstring, unbalanced parentheses, a backslash continuation at
/// EOF) also returns 0 instead of guessing.
#[cfg(feature = "python")]
fn future_import_offset(source: &str) -> usize {
    let mut cursor = 0usize;

    // Skip blank/comment-only lines. A module docstring — an unindented
    // string literal as the first statement — may precede the futures.
    loop {
        let Some((line, next)) = source_line(source, cursor) else {
            return 0;
        };
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            cursor = next;
            continue;
        }
        if line.len() == trimmed.len() && is_string_literal_start(trimmed) {
            match docstring_end(source, cursor, line) {
                Some(end) => cursor = end,
                None => return 0,
            }
        }
        break;
    }

    // Consume consecutive `from __future__ import …` statements; blank lines
    // and comments between them are allowed by CPython.
    let mut insert_at = 0usize;
    while let Some((line, next)) = source_line(source, cursor) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            cursor = next;
            continue;
        }
        if line.len() != trimmed.len() || !is_future_import(trimmed) {
            break;
        }
        let Some(end) = future_statement_end(source, cursor) else {
            return 0;
        };
        insert_at = end;
        cursor = end;
    }
    insert_at
}

/// Split one physical line off `source` at `start`: `(line without '\n', next
/// line start)`. `None` at (or past) the end of the source.
#[cfg(feature = "python")]
fn source_line(source: &str, start: usize) -> Option<(&str, usize)> {
    if start >= source.len() {
        return None;
    }
    let rest = &source[start..];
    match rest.find('\n') {
        Some(offset) => Some((&rest[..offset], start + offset + 1)),
        None => Some((rest, source.len())),
    }
}

/// Length in bytes of a Python string prefix (`r`, `u`, `b`, `f`, `rb`, `fr`,
/// …). Case-insensitive; at most three chars as in CPython's grammar.
#[cfg(feature = "python")]
fn string_prefix_len(text: &str) -> usize {
    let mut length = 0usize;
    for character in text.chars().take(3) {
        if matches!(character, 'r' | 'R' | 'u' | 'U' | 'b' | 'B' | 'f' | 'F') {
            length += character.len_utf8();
        } else {
            break;
        }
    }
    length
}

#[cfg(feature = "python")]
fn is_string_literal_start(text: &str) -> bool {
    let rest = &text[string_prefix_len(text)..];
    rest.starts_with('"') || rest.starts_with('\'')
}

/// End (next line start) of a string-literal statement that begins on the
/// line containing `line_start`. Only a string followed by whitespace or a
/// comment on its final line is accepted; anything else returns `None` so the
/// caller falls back to the old prepend.
#[cfg(feature = "python")]
fn docstring_end(source: &str, line_start: usize, line: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let quote_start = line_start + string_prefix_len(line);
    let quote = *bytes.get(quote_start)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    if bytes.get(quote_start..quote_start + 3) == Some([quote, quote, quote].as_slice()) {
        // Triple-quoted: the docstring may span lines. Backslashes escape the
        // following byte (also in raw strings, where they still prevent the
        // quote from terminating the literal).
        let mut index = quote_start + 3;
        while index + 2 < bytes.len() {
            match bytes[index] {
                b'\\' => index += 2,
                byte if byte == quote && bytes[index + 1] == quote && bytes[index + 2] == quote => {
                    return finish_statement_line(source, index + 3);
                }
                _ => index += 1,
            }
        }
        None
    } else {
        // Single-quoted: must close on the same physical line.
        let mut index = quote_start + 1;
        while index < bytes.len() && bytes[index] != b'\n' {
            match bytes[index] {
                b'\\' => index += 2,
                byte if byte == quote => return finish_statement_line(source, index + 1),
                _ => index += 1,
            }
        }
        None
    }
}

/// Accept only whitespace or a comment between `position` and the end of the
/// line; return the next line start (or EOF).
#[cfg(feature = "python")]
fn finish_statement_line(source: &str, position: usize) -> Option<usize> {
    let rest = &source[position..];
    let line_end = rest.find('\n').map(|offset| position + offset);
    let tail = match line_end {
        Some(end) => source[position..end].trim(),
        None => rest.trim(),
    };
    if !tail.is_empty() && !tail.starts_with('#') {
        return None;
    }
    Some(match line_end {
        Some(end) => end + 1,
        None => source.len(),
    })
}

#[cfg(feature = "python")]
fn is_future_import(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("from __future__ import") else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(' ' | '\t' | '(') => true,
        Some(_) => false,
    }
}

/// End (next line start) of one `from __future__ import …` statement,
/// following parentheses and backslash continuations. Comments are ignored
/// while counting the parens. `None` on an unbalanced statement at EOF.
#[cfg(feature = "python")]
fn future_statement_end(source: &str, line_start: usize) -> Option<usize> {
    let mut cursor = line_start;
    let mut depth: i32 = 0;
    loop {
        let (line, next) = source_line(source, cursor)?;
        let code = line.split('#').next().unwrap_or_default();
        for character in code.chars() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return None;
                    }
                }
                _ => {}
            }
        }
        let continued = code.trim_end().ends_with('\\');
        if depth == 0 && !continued {
            return Some(next);
        }
        if next == cursor {
            return None;
        }
        cursor = next;
    }
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
