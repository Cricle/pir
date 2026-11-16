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

/// One raw match or surrounding context line from the in-process searcher.
pub(crate) struct RawMatch {
    pub(crate) path: PathBuf,
    pub(crate) line_number: u64,
    pub(crate) line_text: String,
    /// True for a surrounding context line (upstream rg `context` event).
    pub(crate) is_context: bool,
}

/// Run-wide collection state shared by every per-file sink: the match limit
/// and its trailing context span files, and the byte budget is a total, not a
/// per-file allowance (upstream grep.ts:184-230 `matchCount`/`limitHit`/
/// `trailingContextEnd`/`lastFile`).
struct CollectState {
    matches: Vec<RawMatch>,
    match_count: usize,
    /// Set at the last allowed match. The searcher still delivers that
    /// match's trailing context, kept up to `trailing_context_end`, and the
    /// limit notice waits for proof of one more match (#11705).
    limit_hit: bool,
    trailing_context_end: u64,
    last_file: Option<PathBuf>,
    /// Retained match/context text bytes the WHOLE search may keep.
    retained_bytes: usize,
    limit_reached: bool,
    byte_limit_reached: bool,
    aborted: bool,
}

/// Collecting sink over one file's matches: enforces the run-wide match
/// limit, the optional retained-text byte budget and the abort flag by
/// returning `false` (stop the search) and recording the reason.
struct CollectSink<'a> {
    path: &'a Path,
    context: usize,
    limit: usize,
    max_bytes: Option<usize>,
    abort: &'a AtomicBool,
    state: &'a mut CollectState,
}

impl CollectSink<'_> {
    /// Trim the line terminator at the BYTE level: CR/LF are ASCII (never part
    /// of a multi-byte sequence), so a lone CR inside the line stays content
    /// (upstream `replace(/\r?\n?$/, "")`).
    fn trimmed(raw: &[u8]) -> &[u8] {
        let mut trimmed_len = raw.len();
        while trimmed_len > 0 && matches!(raw[trimmed_len - 1], b'\n' | b'\r') {
            trimmed_len -= 1;
        }
        &raw[..trimmed_len]
    }

    /// Push one line into the retained set, enforcing the byte budget. A line
    /// whose raw bytes already exceed the remaining budget is rejected before
    /// allocating; lossy decoding can only make it larger, so the second check
    /// still enforces the hard budget.
    fn push(&mut self, line_number: u64, raw: &[u8], is_context: bool) -> bool {
        let trimmed = Self::trimmed(raw);
        if let Some(max_bytes) = self.max_bytes {
            let remaining = max_bytes.saturating_sub(self.state.retained_bytes);
            if trimmed.len() > remaining {
                self.state.byte_limit_reached = true;
                return false;
            }
        }
        let line_text = String::from_utf8_lossy(trimmed).into_owned();
        if let Some(max_bytes) = self.max_bytes {
            let next = self.state.retained_bytes.saturating_add(line_text.len());
            if next > max_bytes {
                self.state.byte_limit_reached = true;
                return false;
            }
            self.state.retained_bytes = next;
        }
        self.state.last_file = Some(self.path.to_path_buf());
        self.state.matches.push(RawMatch {
            path: self.path.to_path_buf(),
            line_number,
            line_text,
            is_context,
        });
        true
    }

    fn in_trailing_window(&self, line_number: u64) -> bool {
        self.state.limit_hit
            && self.state.last_file.as_deref() == Some(self.path)
            && line_number <= self.state.trailing_context_end
    }
}

impl grep_searcher::Sink for CollectSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.abort.load(Ordering::SeqCst) {
            self.state.aborted = true;
            return Ok(false);
        }
        let line_number = mat.line_number().unwrap_or(0);
        let raw = mat.bytes();
        if self.state.limit_hit {
            let in_window = self.in_trailing_window(line_number);
            // A match inside the last allowed match's trailing context is
            // shown as a context line.
            if in_window && !self.push(line_number, raw, true) {
                return Ok(false);
            }
            // Only another match proves that the limit cut results off.
            self.state.limit_reached = true;
            // Keep draining the trailing window; stop once it ends.
            return Ok(in_window && line_number < self.state.trailing_context_end);
        }
        if !self.push(line_number, raw, false) {
            return Ok(false);
        }
        self.state.match_count += 1;
        if self.state.match_count >= self.limit {
            self.state.limit_hit = true;
            self.state.trailing_context_end = line_number.saturating_add(self.context as u64);
        }
        // Do not stop at the limit: the trailing context of the last allowed
        // match and the proof of one more match are still to come.
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        context: &grep_searcher::SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        if self.abort.load(Ordering::SeqCst) {
            self.state.aborted = true;
            return Ok(false);
        }
        let line_number = context.line_number().unwrap_or(0);
        let raw = context.bytes();
        if !self.state.limit_hit {
            return Ok(self.push(line_number, raw, true));
        }
        let in_window = self.in_trailing_window(line_number);
        if in_window && !self.push(line_number, raw, true) {
            return Ok(false);
        }
        // Once an extra match proved the limit, keep the window up to its end
        // and stop on the first line at/after it.
        Ok(!self.state.limit_reached
            || (in_window && line_number < self.state.trailing_context_end))
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
    /// Lines of surrounding context per match (`rg --context N`), emitted as
    /// context lines around the matches.
    pub(crate) context: usize,
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
        context: 0,
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
        context,
        abort,
    } = request;
    // Existence/permission guard: a missing or unreadable root must be a real
    // error, not an empty match set ("No matches found"). This mirrors the
    // metadata check the grep/ls tools perform before dispatching; the
    // standalone `pir grep` command validates too, but the engine is the last
    // line of defense (and the walker alone reports a missing root as a walk
    // error, which used to be discarded).
    std::fs::metadata(root).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            format!("Path not found: {}", super::util::display_path(root))
        }
        _ => format!(
            "Cannot access '{}': {error}",
            super::util::display_path(root)
        ),
    })?;
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .fixed_strings(literal)
        .word(word)
        .build(pattern)
        .map_err(|error| format!("Invalid search pattern: {error}"))?;
    let mut searcher_builder = grep_searcher::SearcherBuilder::new();
    searcher_builder
        .line_number(true)
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'));
    if context > 0 {
        // `rg --context N`: the surrounding lines arrive as context events,
        // so no matched file is ever re-read (upstream #11705).
        searcher_builder
            .before_context(context)
            .after_context(context);
    }
    let mut searcher = searcher_builder.build();

    // RUN-WIDE state: the sink below points every file's search at this one
    // state, so the limit, its trailing context and the retained-text budget
    // are totals rather than per-file allowances.
    let mut state = CollectState {
        matches: Vec::new(),
        match_count: 0,
        limit_hit: false,
        trailing_context_end: 0,
        last_file: None,
        retained_bytes: 0,
        limit_reached: false,
        byte_limit_reached: false,
        aborted: false,
    };
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
            context,
            limit,
            max_bytes: max_match_bytes,
            abort,
            state: &mut state,
        };
        // Read errors on a single file are recorded and skipped while other
        // files remain searchable (rg reports and moves on). If no file could
        // be searched, the caller gets the first error below.
        match searcher.search_path(&matcher, path, &mut sink) {
            Ok(()) => searched_ok += 1,
            Err(error) => {
                if first_search_error.is_none() {
                    first_search_error = Some(format!(
                        "Failed to search '{}': {error}",
                        super::util::display_path(path)
                    ));
                }
            }
        }
        !sink.state.limit_reached && !sink.state.aborted && !sink.state.byte_limit_reached
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
            return Err(format!(
                "Failed to walk '{}': {error}",
                super::util::display_path(root)
            ));
        }
    }
    Ok((
        state.matches,
        state.limit_reached,
        searched_files,
        state.byte_limit_reached,
    ))
}
