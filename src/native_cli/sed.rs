//! `pir sed`: the native sed subset — `-n`, `-e`, `-E`/`-r`, `-i[SUFFIX]`,
//! files or stdin, and the commands `s///`, `d`, `p`, `q`, `=` with the
//! addresses `N`, `$` and `/regex/`.
//!
//! Anything outside the subset (blocks, ranges, `a`/`i`/`c`, `y`, `r`/`w`,
//! branches, negation, `-f`, GNU-only flags, BRE constructs the Rust regex
//! dialect reads differently) falls back to the system `sed` with the
//! original argv.
//!
//! Exit codes: 0 success, 1 script error, 2 file error (the remaining files
//! are still processed).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::RunOutcome;

pub(crate) fn run(args: &[String]) -> RunOutcome {
    let options = match Options::parse(args) {
        Some(options) => options,
        None => return RunOutcome::Unsupported,
    };
    let script_text = options.scripts.join("\n");
    let commands = match parse_script(&script_text, options.extended) {
        Some(commands) => commands,
        None => return RunOutcome::Unsupported,
    };
    if options.in_place.is_some() {
        run_in_place(&options, &commands)
    } else {
        run_stream(&options, &commands)
    }
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

struct Options {
    quiet: bool,
    extended: bool,
    /// `Some(suffix)` when `-i` was given; an empty suffix means "no backup".
    in_place: Option<String>,
    scripts: Vec<String>,
    files: Vec<String>,
}

impl Options {
    fn parse(args: &[String]) -> Option<Self> {
        let mut quiet = false;
        let mut extended = false;
        let mut in_place = None;
        let mut scripts = Vec::new();
        let mut positionals = Vec::new();
        let mut index = 0;
        let mut end_of_options = false;
        while index < args.len() {
            let arg = &args[index];
            if end_of_options {
                positionals.push(arg.clone());
                index += 1;
                continue;
            }
            if arg == "--" {
                end_of_options = true;
                index += 1;
                continue;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, inline) = match long.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_owned())),
                    None => (long, None),
                };
                match name {
                    "quiet" | "silent" => quiet = true,
                    "regexp-extended" => extended = true,
                    "expression" => {
                        let value = match inline {
                            Some(value) => value,
                            None => {
                                index += 1;
                                args.get(index)?.clone()
                            }
                        };
                        scripts.push(value);
                    }
                    "in-place" => in_place = Some(inline.unwrap_or_default()),
                    // -f/--file, --posix, --debug, --sandbox, --follow-symlinks,
                    // --unbuffered, --separate, --null-data, --binary,
                    // --line-length: system sed only.
                    _ => return None,
                }
                index += 1;
                continue;
            }
            if arg.len() > 1 && arg.starts_with('-') {
                let chars: Vec<char> = arg.chars().skip(1).collect();
                let mut offset = 0;
                while offset < chars.len() {
                    match chars[offset] {
                        'n' => quiet = true,
                        'E' | 'r' => extended = true,
                        'e' => {
                            let rest: String = chars[offset + 1..].iter().collect();
                            let value = if rest.is_empty() {
                                index += 1;
                                args.get(index)?.clone()
                            } else {
                                rest
                            };
                            scripts.push(value);
                            offset = chars.len();
                            continue;
                        }
                        'i' => {
                            // `-i` takes an optional suffix attached to the
                            // same argument (`-i.bak`, `-ni.bak`).
                            let suffix: String = chars[offset + 1..].iter().collect();
                            in_place = Some(suffix);
                            offset = chars.len();
                            continue;
                        }
                        _ => return None,
                    }
                    offset += 1;
                }
                index += 1;
                continue;
            }
            positionals.push(arg.clone());
            index += 1;
        }
        if scripts.is_empty() {
            if positionals.is_empty() {
                return None;
            }
            scripts.push(positionals.remove(0));
        }
        if in_place.is_some() && positionals.is_empty() {
            // GNU sed -i without files is a no-op; let the system tool decide.
            return None;
        }
        // `-i*` substitutes the file name in the backup suffix (GNU feature).
        if in_place
            .as_deref()
            .is_some_and(|suffix| suffix.contains('*'))
        {
            return None;
        }
        // `-` names stdin as an input file; the native reader only reads stdin
        // when no file is given.
        if positionals.iter().any(|file| file == "-") {
            return None;
        }
        Some(Self {
            quiet,
            extended,
            in_place,
            scripts,
            files: positionals,
        })
    }
}

// ---------------------------------------------------------------------------
// Script parser
// ---------------------------------------------------------------------------

enum Address {
    Line(u64),
    Last,
    Regex(regex::Regex),
}

enum Action {
    Substitute {
        regex: regex::Regex,
        replacement: String,
        global: bool,
        print: bool,
    },
    Delete,
    Print,
    Quit,
    LineNumber,
}

struct Command {
    address: Option<Address>,
    action: Action,
}

struct Cursor {
    chars: Vec<char>,
    position: usize,
}

impl Cursor {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            position: 0,
        }
    }

    fn eof(&self) -> bool {
        self.position >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += 1;
        Some(character)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.position += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | ';' | '\n')) {
            self.position += 1;
        }
    }

    fn parse_number(&mut self) -> Option<u64> {
        let mut value: u64 = 0;
        let mut digits = 0;
        while let Some(character) = self.peek() {
            let Some(digit) = character.to_digit(10) else {
                break;
            };
            value = value.checked_mul(10)?.checked_add(digit as u64)?;
            digits += 1;
            self.position += 1;
        }
        (digits > 0).then_some(value)
    }

    /// Read a delimited field; the opening delimiter must already be consumed.
    /// `\<delim>` becomes a literal delimiter, every other escape is kept for
    /// the regex/replacement layers.
    fn read_until(&mut self, delimiter: char) -> Option<String> {
        let mut out = String::new();
        while let Some(character) = self.bump() {
            match character {
                '\\' => {
                    let next = self.bump()?;
                    if next == delimiter {
                        out.push(delimiter);
                    } else {
                        out.push('\\');
                        out.push(next);
                    }
                }
                character if character == delimiter => return Some(out),
                character => out.push(character),
            }
        }
        None
    }

    fn parse_command(&mut self, extended: bool) -> Option<Command> {
        let address = self.parse_address(extended)?;
        self.skip_blanks();
        if self.eof() || matches!(self.peek(), Some(';' | '\n' | '\r')) {
            // An address without a command is not in the native subset.
            return None;
        }
        let action = match self.bump()? {
            's' => self.parse_substitute(extended)?,
            'd' => Action::Delete,
            'p' => Action::Print,
            'q' => Action::Quit,
            '=' => Action::LineNumber,
            // `{`, `a`, `i`, `c`, `y`, `r`, `w`, `b`, `t`, `T`, `:`, `l`, …
            _ => return None,
        };
        Some(Command { address, action })
    }

    fn parse_address(&mut self, extended: bool) -> Option<Option<Address>> {
        match self.peek() {
            Some(character) if character.is_ascii_digit() => {
                let line = self.parse_number()?;
                if line == 0 {
                    return None;
                }
                Some(Some(Address::Line(line)))
            }
            Some('$') => {
                self.position += 1;
                Some(Some(Address::Last))
            }
            Some('/') => {
                self.position += 1;
                let pattern = self.read_until('/')?;
                if pattern.is_empty() {
                    // `//` reuses the last regex: not modeled natively.
                    return None;
                }
                let pattern = if extended {
                    pattern
                } else {
                    super::bre_to_regex(&pattern)?
                };
                let regex = regex::Regex::new(&pattern).ok()?;
                Some(Some(Address::Regex(regex)))
            }
            _ => Some(None),
        }
    }

    fn parse_substitute(&mut self, extended: bool) -> Option<Action> {
        let delimiter = self.peek()?;
        if matches!(delimiter, '\\' | '\n' | ';') {
            return None;
        }
        self.position += 1;
        let pattern = self.read_until(delimiter)?;
        let replacement = self.read_until(delimiter)?;
        if pattern.is_empty() {
            return None;
        }
        let pattern = if extended {
            pattern
        } else {
            super::bre_to_regex(&pattern)?
        };
        let mut global = false;
        let mut print = false;
        let mut case_insensitive = false;
        while let Some(character) = self.peek() {
            match character {
                'g' => {
                    global = true;
                    self.position += 1;
                }
                'p' => {
                    print = true;
                    self.position += 1;
                }
                'i' | 'I' => {
                    case_insensitive = true;
                    self.position += 1;
                }
                ' ' | '\t' => self.position += 1,
                ';' | '\n' | '\r' => break,
                // Numeric flags and GNU's m/M: system sed only.
                _ => return None,
            }
        }
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()
            .ok()?;
        let replacement = translate_replacement(&replacement)?;
        Some(Action::Substitute {
            regex,
            replacement,
            global,
            print,
        })
    }
}

fn parse_script(text: &str, extended: bool) -> Option<Vec<Command>> {
    let mut cursor = Cursor::new(text);
    let mut commands = Vec::new();
    loop {
        cursor.skip_separators();
        if cursor.eof() {
            break;
        }
        commands.push(cursor.parse_command(extended)?);
        cursor.skip_blanks();
        if !cursor.eof() && !matches!(cursor.peek(), Some(';' | '\n' | '\r')) {
            return None;
        }
    }
    Some(commands)
}

/// Translate a sed replacement into the `regex` crate's `Captures::expand`
/// syntax (`$0`, `${1}`, `$$`). Escapes outside the supported set make the
/// script fall back to the system sed.
fn translate_replacement(replacement: &str) -> Option<String> {
    let mut translated = String::new();
    let mut chars = replacement.chars();
    while let Some(character) = chars.next() {
        match character {
            '&' => translated.push_str("$0"),
            '$' => translated.push_str("$$"),
            '\\' => match chars.next()? {
                digit @ '1'..='9' => {
                    translated.push_str("${");
                    translated.push(digit);
                    translated.push('}');
                }
                'n' => translated.push('\n'),
                't' => translated.push('\t'),
                '\\' => translated.push('\\'),
                '&' => translated.push('&'),
                '/' => translated.push('/'),
                // \L, \U, \l, \u, \E, \0, unknown escapes: system sed only.
                _ => return None,
            },
            character => translated.push(character),
        }
    }
    Some(translated)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

fn address_matches(address: Option<&Address>, number: u64, last: u64, pattern: &str) -> bool {
    match address {
        None => true,
        Some(Address::Line(line)) => number == *line,
        Some(Address::Last) => number == last,
        Some(Address::Regex(regex)) => regex.is_match(pattern),
    }
}

fn substitute(regex: &regex::Regex, text: &str, replacement: &str, global: bool) -> (String, bool) {
    let mut output = String::new();
    let mut last_end = 0;
    let mut replaced = false;
    for captures in regex.captures_iter(text) {
        let Some(found) = captures.get(0) else {
            continue;
        };
        output.push_str(&text[last_end..found.start()]);
        let mut expanded = String::new();
        captures.expand(replacement, &mut expanded);
        output.push_str(&expanded);
        last_end = found.end();
        replaced = true;
        if !global {
            break;
        }
    }
    output.push_str(&text[last_end..]);
    (output, replaced)
}

/// Split text into `(line, had_terminator)` pairs. A final line without a
/// newline is kept (sed prints it without a trailing newline). Only `\n`
/// separates lines: a `\r` before it stays part of the line, so CRLF input is
/// rewritten as CRLF (GNU sed's binary-neutral behavior).
fn split_lines(text: &str) -> Vec<(String, bool)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            lines.push((text[start..index].to_owned(), true));
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push((text[start..].to_owned(), false));
    }
    lines
}

fn process(
    lines: &[(String, bool)],
    commands: &[Command],
    quiet: bool,
    first_line: u64,
    last_line: u64,
) -> (String, bool) {
    let last = last_line;
    // Each emitted chunk carries whether the input line it came from was
    // newline-terminated; GNU sed inserts a newline between chunks when the
    // previous one was not, and leaves the very last chunk unterminated.
    let mut chunks: Vec<(String, bool)> = Vec::new();
    let mut quit_requested = false;
    for (index, (text, terminated)) in lines.iter().enumerate() {
        let number = first_line + index as u64;
        let mut pattern = text.clone();
        let mut deleted = false;
        let mut quit = false;
        for command in commands {
            if !address_matches(command.address.as_ref(), number, last, &pattern) {
                continue;
            }
            match &command.action {
                Action::Substitute {
                    regex,
                    replacement,
                    global,
                    print,
                } => {
                    let (substituted, replaced) = substitute(regex, &pattern, replacement, *global);
                    pattern = substituted;
                    // `s///p` prints the pattern space only when this line
                    // actually had a substitution (GNU sed).
                    if *print && replaced {
                        chunks.push((pattern.clone(), *terminated));
                    }
                }
                Action::Delete => {
                    deleted = true;
                    break;
                }
                Action::Print => {
                    chunks.push((pattern.clone(), *terminated));
                }
                Action::Quit => {
                    quit = true;
                    break;
                }
                Action::LineNumber => {
                    chunks.push((number.to_string(), true));
                }
            }
        }
        if !deleted && !quiet {
            chunks.push((pattern, *terminated));
        }
        if quit {
            quit_requested = true;
            break;
        }
    }

    let mut output = String::new();
    for (index, (text, terminated)) in chunks.iter().enumerate() {
        output.push_str(text);
        if *terminated || index + 1 < chunks.len() {
            output.push('\n');
        }
    }
    (output, quit_requested)
}

fn io_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        _ => error.to_string(),
    }
}

/// Per-file / per-stream input cap. The native `sed` path materializes the
/// input, its split lines and the output; a multi-GB file would need several
/// GB of RSS before producing anything. GNU sed streams line by line, but
/// this path deliberately stays a small text tool (the same hardening
/// philosophy as the 64 MiB `@file` cap); larger inputs must use the system
/// sed fallback.
const MAX_SED_INPUT_BYTES: u64 = 128 * 1024 * 1024;

fn read_file(path: &str) -> Result<String, ReadFailure> {
    let metadata =
        std::fs::metadata(path).map_err(|error| ReadFailure::Missing(io_message(&error)))?;
    if metadata.len() > MAX_SED_INPUT_BYTES {
        return Err(ReadFailure::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(|error| ReadFailure::Missing(io_message(&error)))?;
    if bytes.len() as u64 > MAX_SED_INPUT_BYTES {
        return Err(ReadFailure::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| ReadFailure::NotText)
}

enum ReadFailure {
    Missing(String),
    NotText,
    TooLarge,
}

fn run_stream(options: &Options, commands: &[Command]) -> RunOutcome {
    if options.files.is_empty() {
        let mut bytes = Vec::new();
        let mut stdin = std::io::stdin().lock();
        if stdin
            .by_ref()
            .take(MAX_SED_INPUT_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .is_err()
        {
            return RunOutcome::Unsupported;
        }
        if bytes.len() as u64 > MAX_SED_INPUT_BYTES {
            eprintln!("sed: input too large (limit {MAX_SED_INPUT_BYTES} bytes)");
            return RunOutcome::Exit(2);
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            // stdin is already at EOF here, so the fallback must receive the
            // original bytes through a pipe instead of inheriting it.
            Err(error) => return RunOutcome::UnsupportedWithStdin(error.into_bytes()),
        };
        let lines = split_lines(&text);
        let (output, _quit) = process(&lines, commands, options.quiet, 1, lines.len() as u64);
        let _ = std::io::stdout().write_all(output.as_bytes());
        return RunOutcome::Exit(0);
    }

    // Validation/count pass: a non-UTF-8 file must fall back before any
    // output is produced, and `$` needs the total line count of the whole
    // file stream. Each file is held alone, then dropped.
    let mut errors: Vec<String> = Vec::new();
    let mut total_lines: u64 = 0;
    for file in &options.files {
        match read_file(file) {
            Ok(text) => {
                total_lines = total_lines.saturating_add(split_lines(&text).len() as u64);
            }
            Err(ReadFailure::Missing(message)) => {
                errors.push(format!("sed: can't read {file}: {message}"));
            }
            Err(ReadFailure::NotText) => return RunOutcome::Unsupported,
            Err(ReadFailure::TooLarge) => return RunOutcome::Unsupported,
        }
    }
    // Process one file at a time: peak RSS is bounded by the largest file,
    // not the sum of every input plus a concatenated copy. Line numbers run
    // continuously across files, matching GNU sed.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut first_line: u64 = 1;
    let mut quit = false;
    let mut pending_cross_file_newline = false;
    for file in &options.files {
        if quit {
            break;
        }
        let Ok(text) = read_file(file) else {
            errors.push(format!("sed: can't read {file}"));
            continue;
        };
        let lines = split_lines(&text);
        let (output, file_quit) = process(&lines, commands, options.quiet, first_line, total_lines);
        if !output.is_empty() {
            if pending_cross_file_newline {
                let _ = out.write_all(b"\n");
            }
            let _ = out.write_all(output.as_bytes());
            pending_cross_file_newline = !output.ends_with('\n');
        }
        first_line = first_line.saturating_add(lines.len() as u64);
        quit = file_quit;
    }

    for message in &errors {
        eprintln!("{message}");
    }
    if errors.is_empty() {
        RunOutcome::Exit(0)
    } else {
        RunOutcome::Exit(2)
    }
}

fn run_in_place(options: &Options, commands: &[Command]) -> RunOutcome {
    let suffix = options.in_place.as_deref().unwrap_or_default();
    let mut errors: Vec<String> = Vec::new();
    // Validate every file before rewriting any of them (a non-UTF-8 or
    // oversized file must degrade before the first write-back), but do not
    // keep the contents: the processing pass re-reads one file at a time so
    // peak RSS is the largest file, not the sum of the whole argument list.
    for file in &options.files {
        match read_file(file) {
            Ok(_) => {}
            Err(ReadFailure::Missing(message)) => {
                errors.push(format!("sed: can't read {file}: {message}"));
            }
            Err(ReadFailure::NotText) => return RunOutcome::Unsupported,
            Err(ReadFailure::TooLarge) => return RunOutcome::Unsupported,
        }
    }
    for file in &options.files {
        let Ok(text) = read_file(file) else {
            continue;
        };
        let lines = split_lines(&text);
        let (output, quit) = process(&lines, commands, options.quiet, 1, lines.len() as u64);
        if let Err(error) = write_in_place(Path::new(file), &output, suffix) {
            errors.push(format!("sed: couldn't write {file}: {error}"));
        }
        // GNU sed's `q` terminates the whole invocation: files after the one
        // that quit are left exactly as they were (not truncated to the quit
        // line). `process` scopes line addressing per file, but `q` is not a
        // per-file reset.
        if quit {
            break;
        }
    }
    for message in &errors {
        eprintln!("{message}");
    }
    if errors.is_empty() {
        RunOutcome::Exit(0)
    } else {
        RunOutcome::Exit(2)
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `content` through a temp file in the same directory
/// (the Windows-safe write-back), optionally keeping a `suffix` backup.
fn write_in_place(path: &Path, content: &str, suffix: &str) -> std::io::Result<()> {
    if !suffix.is_empty() {
        let mut backup = path.as_os_str().to_os_string();
        backup.push(suffix);
        std::fs::copy(path, PathBuf::from(backup))?;
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sed".to_owned());
    let temp = parent.join(format!(
        ".{name}.pir-sed-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        drop(file);
        let permissions = std::fs::metadata(path)?.permissions();
        std::fs::set_permissions(&temp, permissions)?;
        // `std::fs::rename` replaces the destination on both platforms
        // (Windows: MoveFileExW with MOVEFILE_REPLACE_EXISTING), so a failed
        // replace never leaves the original deleted.
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
