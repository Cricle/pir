//! `pir grep`: GNU-grep-compatible front end over the shared in-process
//! ripgrep engine ([`crate::coding_agent::tools::grep_engine`]) — the same
//! walk/match loop the `grep` tool uses.
//!
//! Supported natively: `-i`, `-F`, `-n`, `-r`/`-R`, `-l`, `-c`, `-w`,
//! `--include=GLOB`, `--exclude=GLOB`, `-C N`, `--color=never`, and one or
//! more file or directory arguments (a directory walks recursively).
//! Everything else (`-v`, `-A`/`-B`, `-E`/`-G`/`-P`, `--exclude-dir`, `-e`,
//! reading stdin, BRE backreferences, …) falls back to the system `grep` with
//! the original argv.

use super::RunOutcome;

#[cfg(feature = "grep-find")]
use std::path::PathBuf;

#[cfg(feature = "grep-find")]
use std::sync::atomic::AtomicBool;

#[cfg(feature = "grep-find")]
use crate::coding_agent::tools::grep_engine::{search_sync, RawMatch, SearchRequest};

#[cfg(feature = "grep-find")]
pub(crate) fn run(args: &[String]) -> RunOutcome {
    match Options::parse(args) {
        Some(options) => execute(options),
        None => RunOutcome::Unsupported,
    }
}

#[cfg(not(feature = "grep-find"))]
pub(crate) fn run(_args: &[String]) -> RunOutcome {
    // The engine crates ride the `grep-find` feature; without them the whole
    // command is the system grep.
    RunOutcome::Unsupported
}

/// Parsed native subset of the GNU grep command line.
#[cfg(feature = "grep-find")]
#[derive(Default)]
struct Options {
    ignore_case: bool,
    fixed: bool,
    line_number: bool,
    recursive: bool,
    files_with_matches: bool,
    count: bool,
    word: bool,
    context: usize,
    include: Vec<String>,
    exclude: Vec<String>,
    pattern: String,
    paths: Vec<String>,
}

#[cfg(feature = "grep-find")]
impl Options {
    fn parse(args: &[String]) -> Option<Self> {
        let mut options = Options::default();
        let mut positionals: Vec<String> = Vec::new();
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
            // `-` means stdin: no native stdin search.
            if arg == "-" {
                return None;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, inline) = match long.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_owned())),
                    None => (long, None),
                };
                match name {
                    "ignore-case" => options.ignore_case = true,
                    "fixed-strings" => options.fixed = true,
                    "line-number" => options.line_number = true,
                    "recursive" | "dereference-recursive" => options.recursive = true,
                    "files-with-matches" => options.files_with_matches = true,
                    "count" => options.count = true,
                    "word-regexp" => options.word = true,
                    "include" | "exclude" | "context" => {
                        let value = match inline {
                            Some(value) => value,
                            None => {
                                index += 1;
                                args.get(index)?.clone()
                            }
                        };
                        match name {
                            "include" => options.include.push(value),
                            "exclude" => options.exclude.push(value),
                            _ => options.context = value.parse().ok()?,
                        }
                    }
                    // `--color=never` is a no-op; every other mode changes the
                    // byte stream and is left to the system tool.
                    "color" | "colour" => match inline.as_deref() {
                        Some("never") => {}
                        _ => return None,
                    },
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
                        'i' => options.ignore_case = true,
                        'F' => options.fixed = true,
                        'n' => options.line_number = true,
                        'r' | 'R' => options.recursive = true,
                        'l' => options.files_with_matches = true,
                        'c' => options.count = true,
                        'w' => options.word = true,
                        'C' => {
                            let rest: String = chars[offset + 1..].iter().collect();
                            let value = if rest.is_empty() {
                                index += 1;
                                args.get(index)?.clone()
                            } else {
                                rest
                            };
                            options.context = value.parse().ok()?;
                            offset = chars.len();
                            continue;
                        }
                        // -v, -A/-B, -E/-G/-P, -e, -o, -q, -s, -H, -h, …
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
        if positionals.is_empty() {
            return None;
        }
        options.pattern = positionals.remove(0);
        options.paths = positionals;
        // No path means stdin (or the recursive default `.`): not native.
        if options.paths.is_empty() {
            return None;
        }
        Some(options)
    }
}

#[cfg(feature = "grep-find")]
struct FileMatches {
    path: PathBuf,
    matches: Vec<RawMatch>,
}

#[cfg(feature = "grep-find")]
fn execute(mut options: Options) -> RunOutcome {
    use std::io::Write;

    // The default dialect is BRE; translate it into the engine's regex
    // syntax. A pattern the native engine cannot reproduce (backreference,
    // escaped bracket expression) falls back instead of silently changing
    // semantics.
    if !options.fixed {
        match super::bre_to_regex(&options.pattern) {
            Some(translated) => options.pattern = translated,
            None => return RunOutcome::Unsupported,
        }
    }

    // Validate every path before producing output, so a fallback never
    // repeats an error that was already printed.
    let mut targets: Vec<(PathBuf, bool)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for raw in &options.paths {
        let path = PathBuf::from(raw);
        match std::fs::metadata(&path) {
            // A directory argument walks recursively (rg's ergonomics; `-r`
            // stays accepted and means the same thing).
            Ok(metadata) if metadata.is_dir() => targets.push((path, true)),
            Ok(_) => targets.push((path, false)),
            Err(error) => errors.push(format!("grep: {raw}: {}", io_message(&error))),
        }
    }

    let walked = options.recursive || targets.iter().any(|(_, is_directory)| *is_directory);
    let multiple = walked || options.paths.len() > 1;
    let abort = AtomicBool::new(false);
    let mut files: Vec<FileMatches> = Vec::new();
    let mut positions: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for (path, is_directory) in &targets {
        let (found, _limit, searched) = match search_sync(SearchRequest {
            root: path,
            is_directory: *is_directory,
            pattern: &options.pattern,
            ignore_case: options.ignore_case,
            literal: options.fixed,
            word: options.word,
            globs: &options.include,
            excludes: &options.exclude,
            // GNU grep searches ignored/hidden files too.
            respect_ignore: false,
            limit: usize::MAX,
            abort: &abort,
        }) {
            Ok(result) => result,
            // The engine rejects patterns/globs the system grep may accept.
            Err(_) => return RunOutcome::Unsupported,
        };
        // Seed one entry per searched file so `-c` can report zero counts in
        // walk order.
        for searched_path in searched {
            if positions.contains_key(&searched_path) {
                continue;
            }
            positions.insert(searched_path.clone(), files.len());
            files.push(FileMatches {
                path: searched_path,
                matches: Vec::new(),
            });
        }
        for found_match in found {
            if let Some(&position) = positions.get(&found_match.path) {
                files[position].matches.push(found_match);
            }
        }
    }

    let had_error = !errors.is_empty();
    for message in &errors {
        eprintln!("{message}");
    }

    let mut output = String::new();
    for file in &files {
        let display = display_path(&file.path, walked);
        if options.files_with_matches {
            if file.matches.is_empty() {
                continue;
            }
            output.push_str(&display);
            output.push('\n');
            continue;
        }
        if options.count {
            if multiple {
                output.push_str(&format!("{display}:{}\n", file.matches.len()));
            } else {
                output.push_str(&format!("{}\n", file.matches.len()));
            }
            continue;
        }
        if file.matches.is_empty() {
            continue;
        }
        // GNU prints `--` only between context groups of the SAME file.
        let mut first_group = true;
        render_matches(
            &mut output,
            &options,
            multiple,
            &display,
            file,
            &mut first_group,
        );
    }

    let _ = std::io::stdout().write_all(output.as_bytes());
    let matched = files.iter().any(|file| !file.matches.is_empty());
    if had_error {
        RunOutcome::Exit(2)
    } else if matched {
        RunOutcome::Exit(0)
    } else {
        RunOutcome::Exit(1)
    }
}

/// Render a path for output. A recursive walk prints `/`-separated paths
/// (GNU grep on a POSIX host); an explicitly named file keeps the spelling
/// the caller used.
#[cfg(feature = "grep-find")]
fn display_path(path: &std::path::Path, walked: bool) -> String {
    let rendered = path.to_string_lossy();
    if walked {
        rendered.replace('\\', "/")
    } else {
        rendered.into_owned()
    }
}

#[cfg(feature = "grep-find")]
fn io_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        _ => error.to_string(),
    }
}

#[cfg(feature = "grep-find")]
fn render_matches(
    output: &mut String,
    options: &Options,
    multiple: bool,
    display: &str,
    file: &FileMatches,
    first_group: &mut bool,
) {
    if options.context == 0 {
        for found in &file.matches {
            let text = found.line_text.trim_end_matches(['\n', '\r']);
            match (options.line_number, multiple) {
                (true, true) => {
                    output.push_str(&format!("{display}:{}:{text}\n", found.line_number))
                }
                (true, false) => output.push_str(&format!("{}:{text}\n", found.line_number)),
                (false, true) => output.push_str(&format!("{display}:{text}\n")),
                (false, false) => output.push_str(&format!("{text}\n")),
            }
        }
        return;
    }

    // Context blocks need the surrounding file lines. An unreadable file
    // degrades to its match lines (the matches themselves are already known).
    let content = match std::fs::read_to_string(&file.path) {
        Ok(content) => content,
        Err(_) => {
            for found in &file.matches {
                let text = found.line_text.trim_end_matches(['\n', '\r']);
                if multiple {
                    output.push_str(&format!("{display}:{text}\n"));
                } else {
                    output.push_str(&format!("{text}\n"));
                }
            }
            return;
        }
    };
    let lines: Vec<&str> = content.lines().collect();
    let context = options.context as u64;

    // Merge overlapping/adjacent context windows (GNU prints one block).
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for found in &file.matches {
        let start = found.line_number.saturating_sub(context).max(1);
        let end = found.line_number + context;
        match ranges.last_mut() {
            Some((_, last_end)) if start <= *last_end + 1 => {
                *last_end = (*last_end).max(end);
            }
            _ => ranges.push((start, end)),
        }
    }

    for (start, end) in ranges {
        if !*first_group {
            output.push_str("--\n");
        }
        *first_group = false;
        let end = end.min(lines.len() as u64);
        for number in start..=end {
            let text = lines[(number - 1) as usize];
            let is_match = file
                .matches
                .binary_search_by_key(&number, |found| found.line_number)
                .is_ok();
            let separator = if is_match { ':' } else { '-' };
            match (options.line_number, multiple) {
                (true, true) => {
                    output.push_str(&format!("{display}{separator}{number}{separator}{text}\n"))
                }
                (true, false) => output.push_str(&format!("{number}{separator}{text}\n")),
                (false, true) => output.push_str(&format!("{display}{separator}{text}\n")),
                (false, false) => output.push_str(&format!("{text}\n")),
            }
        }
    }
}
