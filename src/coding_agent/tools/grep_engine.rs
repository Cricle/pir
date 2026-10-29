//! Shared in-process ripgrep search core: the walk + match loop used by both
//! the `grep` tool and the standalone `pir grep` command (single source of
//! truth, no duplicated search logic).
//!
//! The engine runs ripgrep's own crates (`grep-searcher` / `grep-regex` /
//! `ignore`) in-process instead of spawning an external binary. A walk or
//! per-file read error is skipped while other files remain searchable (rg's
//! best-effort behavior); if nothing could be searched at all, the first
//! error is returned instead of a misleading empty result. Pattern and glob
//! errors are reported to the caller.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// One raw match from the in-process searcher.
pub(crate) struct RawMatch {
    pub(crate) path: PathBuf,
    pub(crate) line_number: u64,
    pub(crate) line_text: String,
}

/// Collecting sink over one file's matches: enforces the global match limit,
/// the optional retained-text byte budget and the abort flag by returning
/// `false` (stop this file) and recording the reason.
///
/// The byte budget is SHARED across the whole walk: `retained_bytes` points at
/// the run-wide counter, never at a per-file one (a per-file reset would turn
/// one total budget into `budget × files`). `max_bytes: None` is the CLI's
/// unbounded-text path.
struct CollectSink<'a> {
    path: &'a Path,
    matches: &'a mut Vec<RawMatch>,
    limit: usize,
    limit_reached: bool,
    /// Match-text bytes the WHOLE search may retain (see [`CollectSink`]).
    max_bytes: Option<usize>,
    retained_bytes: &'a mut usize,
    byte_limit_reached: bool,
    abort: &'a AtomicBool,
    aborted: bool,
}

impl grep_searcher::Sink for CollectSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.abort.load(Ordering::SeqCst) {
            self.aborted = true;
            return Ok(false);
        }
        let line_number = mat.line_number().unwrap_or(0);
        // Trim trailing CR/LF at the BYTE level first: they are ASCII (never
        // part of a multi-byte sequence), so this matches
        // `trim_end_matches(['\n', '\r'])` on the decoded text without
        // materializing it. A line whose raw bytes already exceed the
        // remaining budget is rejected here; lossy decoding can only make it
        // larger (an invalid byte becomes a 3-byte U+FFFD), so the second
        // check below still enforces the hard budget.
        let raw = mat.bytes();
        let mut trimmed_len = raw.len();
        while trimmed_len > 0 && matches!(raw[trimmed_len - 1], b'\n' | b'\r') {
            trimmed_len -= 1;
        }
        let trimmed = &raw[..trimmed_len];
        if let Some(max_bytes) = self.max_bytes {
            let remaining = max_bytes.saturating_sub(*self.retained_bytes);
            if trimmed.len() > remaining {
                // Stop BEFORE allocating the line: the budget is a hard
                // retained-memory bound, and the caller reports the cap.
                self.byte_limit_reached = true;
                return Ok(false);
            }
        }
        let line_text = String::from_utf8_lossy(trimmed).into_owned();
        if let Some(max_bytes) = self.max_bytes {
            let next = self.retained_bytes.saturating_add(line_text.len());
            if next > max_bytes {
                // Lossy decoding expanded invalid bytes past the budget.
                self.byte_limit_reached = true;
                return Ok(false);
            }
            *self.retained_bytes = next;
        }
        self.matches.push(RawMatch {
            path: self.path.to_path_buf(),
            line_number,
            line_text,
        });
        if self.matches.len() >= self.limit {
            self.limit_reached = true;
            return Ok(false);
        }
        Ok(true)
    }
}

/// Search parameters for [`search_sync`] / [`search_matches`].
pub(crate) struct SearchRequest<'a> {
    pub(crate) root: &'a Path,
    pub(crate) is_directory: bool,
    pub(crate) pattern: &'a str,
    pub(crate) ignore_case: bool,
    pub(crate) literal: bool,
    /// `grep -w`: whole-word matches.
    pub(crate) word: bool,
    /// `--include`-style whitelist globs (empty means "everything").
    pub(crate) globs: &'a [String],
    /// `--exclude`-style globs (applied as `!glob` overrides).
    pub(crate) excludes: &'a [String],
    /// Whether `.gitignore` / `.ignore` rules apply. The grep tool honors
    /// them; `pir grep` mirrors GNU grep and searches ignored files too.
    pub(crate) respect_ignore: bool,
    pub(crate) limit: usize,
    pub(crate) abort: &'a AtomicBool,
}

/// Run the in-process ripgrep search (`rg --hidden --line-number` equivalent).
/// Returns the matches (capped at `limit`), whether the cap stopped the
/// search, and the files that were searched in walk order (needed for
/// `grep -c`, which reports a zero count for non-matching files).
#[cfg(feature = "cli")]
pub(crate) fn search_sync(
    request: SearchRequest<'_>,
) -> Result<(Vec<RawMatch>, bool, Vec<PathBuf>), String> {
    // The CLI keeps its previous unbounded-text behavior (it renders through
    // its own pipeline): no retained-byte budget here.
    let (matches, limit_reached, searched_files, _) = search_sync_impl(request, true, None)?;
    Ok((matches, limit_reached, searched_files))
}

/// Like [`search_sync`], but without collecting the per-file walk list. The
/// grep tool never reads it, so skipping it avoids one `PathBuf` allocation
/// per walked file. `max_match_bytes` bounds the retained match text: the
/// searcher stops (and reports `true` as the third result) instead of
/// accumulating whole lines without limit; the grep tool passes its display
/// collection budget so a broad search on long lines cannot balloon memory.
pub(crate) fn search_matches(
    request: SearchRequest<'_>,
    max_match_bytes: usize,
) -> Result<(Vec<RawMatch>, bool, bool), String> {
    let (matches, limit_reached, _, byte_limit_reached) =
        search_sync_impl(request, false, Some(max_match_bytes))?;
    Ok((matches, limit_reached, byte_limit_reached))
}

/// Test seam: run one bounded directory search and report
/// `(match_count, retained_text_bytes, byte_limit_reached)`.
///
/// Integration tests live in `tests/` (inline test modules are forbidden in
/// `src/`), so this keeps the shared byte budget directly
/// testable without exposing the search internals.
#[doc(hidden)]
pub fn search_budget_for_tests(
    root: &Path,
    pattern: &str,
    max_match_bytes: usize,
) -> Result<(usize, usize, bool), String> {
    let abort = AtomicBool::new(false);
    let request = SearchRequest {
        root,
        is_directory: true,
        pattern,
        ignore_case: false,
        literal: true,
        word: false,
        globs: &[],
        excludes: &[],
        respect_ignore: false,
        limit: 100_000,
        abort: &abort,
    };
    let (matches, _limit_reached, byte_limit_reached) = search_matches(request, max_match_bytes)?;
    let retained = matches.iter().map(|found| found.line_text.len()).sum();
    Ok((matches.len(), retained, byte_limit_reached))
}

fn search_sync_impl(
    request: SearchRequest<'_>,
    collect_searched: bool,
    max_match_bytes: Option<usize>,
) -> Result<(Vec<RawMatch>, bool, Vec<PathBuf>, bool), String> {
    let SearchRequest {
        root,
        is_directory,
        pattern,
        ignore_case,
        literal,
        word,
        globs,
        excludes,
        respect_ignore,
        limit,
        abort,
    } = request;
    // Existence/permission guard: a missing or unreadable root must be a real
    // error, not an empty match set ("No matches found"). This mirrors the
    // metadata check the grep/ls tools perform before dispatching; the
    // standalone `pir grep` command validates too, but the engine is the last
    // line of defense (and the walker alone reports a missing root as a walk
    // error, which used to be discarded).
    std::fs::metadata(root).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("Path not found: {}", root.display()),
        _ => format!("Cannot access '{}': {error}", root.display()),
    })?;
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .fixed_strings(literal)
        .word(word)
        .build(pattern)
        .map_err(|error| format!("Invalid search pattern: {error}"))?;
    let mut searcher = grep_searcher::SearcherBuilder::new()
        .line_number(true)
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'))
        .build();

    let mut matches: Vec<RawMatch> = Vec::new();
    let mut limit_reached = false;
    let mut byte_limit_reached = false;
    // RUN-WIDE retained match text: the sink below points every file's search
    // at this one counter, so `max_match_bytes` is a total budget rather than a
    // per-file allowance (the per-file reset this replaced allowed
    // `budget × files` retained bytes).
    let mut retained_bytes = 0usize;
    let mut searched_files: Vec<PathBuf> = Vec::new();
    let mut first_walk_error: Option<String> = None;
    let mut first_search_error: Option<String> = None;
    let mut searched_ok = 0usize;

    let mut search_one = |path: &Path| -> bool {
        if collect_searched {
            searched_files.push(path.to_path_buf());
        }
        let mut sink = CollectSink {
            path,
            matches: &mut matches,
            limit,
            limit_reached: false,
            max_bytes: max_match_bytes,
            retained_bytes: &mut retained_bytes,
            byte_limit_reached: false,
            abort,
            aborted: false,
        };
        // Read errors on a single file are recorded and skipped while other
        // files remain searchable (rg reports and moves on). If no file could
        // be searched, the caller gets the first error below.
        match searcher.search_path(&matcher, path, &mut sink) {
            Ok(()) => searched_ok += 1,
            Err(error) => {
                if first_search_error.is_none() {
                    first_search_error =
                        Some(format!("Failed to search '{}': {error}", path.display()));
                }
            }
        }
        if sink.limit_reached {
            limit_reached = true;
        }
        if sink.byte_limit_reached {
            byte_limit_reached = true;
        }
        !limit_reached && !sink.aborted && !sink.byte_limit_reached
    };

    if is_directory {
        let mut builder = ignore::WalkBuilder::new(root);
        // `rg --hidden`: search hidden files (the walker's default skips
        // them); .gitignore/.ignore/.git/info/exclude stay respected.
        builder.hidden(false);
        // `rg --hidden` also descends into `.git`, which leaks credentials
        // from `.git/config` and reads pack files. Prune it during traversal
        // (not merely from results). Depth 0 is exempt so explicitly searching
        // a `.git` directory (or a file inside it) still works.
        builder.filter_entry(|entry| entry.depth() == 0 || entry.file_name() != OsStr::new(".git"));
        // Honor .gitignore even outside a git repository (ripgrep's practical
        // behavior; the crate defaults to requiring a `.git` directory).
        builder.require_git(false);
        if !respect_ignore {
            // GNU grep semantics: every file is searched regardless of
            // ignore files.
            builder
                .ignore(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .parents(false);
        }
        if !globs.is_empty() || !excludes.is_empty() {
            let mut overrides = ignore::overrides::OverrideBuilder::new(root);
            for glob in globs {
                overrides
                    .add(glob)
                    .map_err(|error| format!("Invalid glob: {error}"))?;
            }
            for glob in excludes {
                overrides
                    .add(&format!("!{glob}"))
                    .map_err(|error| format!("Invalid glob: {error}"))?;
            }
            let overrides = overrides
                .build()
                .map_err(|error| format!("Invalid glob: {error}"))?;
            builder.overrides(overrides);
        }
        for entry in builder.build() {
            if abort.load(Ordering::SeqCst) {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // Remember the first error: if no file turns out
                    // searchable, the caller must see it instead of "No
                    // matches found".
                    if first_walk_error.is_none() {
                        first_walk_error = Some(error.to_string());
                    }
                    continue;
                }
            };
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            if !search_one(entry.path()) {
                break;
            }
        }
    } else {
        let _ = globs; // A single-file search has nothing to filter.
        let _ = excludes;
        let _ = search_one(root);
    }
    // An unreadable root/path (or a tree where every candidate failed) must
    // surface as an error: an empty result would falsely claim the pattern was
    // absent. Partial failures with at least one searched file stay
    // best-effort, like rg.
    if searched_ok == 0 {
        if let Some(error) = first_search_error {
            return Err(error);
        }
        if let Some(error) = first_walk_error {
            return Err(format!("Failed to walk '{}': {error}", root.display()));
        }
    }
    Ok((matches, limit_reached, searched_files, byte_limit_reached))
}
