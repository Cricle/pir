//! Hand-written, line-oriented syntax highlighter for tool output and
//! markdown fenced code — the `highlightCode` / `getLanguageFromPath` port.
//!
//! Upstream uses `cli-highlight` (highlight.js). Pulling a full grammar
//! engine into the TUI is not worth its weight, so this module implements the
//! subset the chat surface actually shows:
//!
//! - the exact upstream [`getLanguageFromPath`] extension table;
//! - a stateful line scanner (block comments, Python triple strings, JS/Go
//!   template literals) that carries state across lines;
//! - keyword / builtin-type / literal / number / string / comment / operator
//!   / punctuation scopes mapped onto the theme's `syntax*` tokens;
//! - identifier-before-`(` → function, leading uppercase → type;
//! - adjacent same-scope runs coalesced, styled with [`Theme::fg`] only
//!   (never a reset-all escape, so the surrounding tool-box background
//!   survives).
//!
//! Callers pass the color used when no language resolves (markdown fences:
//! `mdCodeBlock`; tool output without a path: `toolOutput`). A recognized
//! language is never auto-detected: upstream deliberately skips
//! `cli-highlight`'s auto-detection, which misidentifies prose, and so do we.
//!
//! Highlighting is capped for very large expanded bodies: beyond
//! [`MAX_HIGHLIGHT_LINES`] / [`MAX_HIGHLIGHT_BYTES`] the tail is emitted
//! verbatim (no escape sequences), keeping a huge read/write render bounded.

use super::theme::{with_theme, Theme, ThemeColor};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Expanded bodies stop being highlighted past this many lines; the tail is
/// left plain.
pub const MAX_HIGHLIGHT_LINES: usize = 5_000;
/// Expanded bodies stop being highlighted past this many bytes; the tail is
/// left plain.
pub const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// Language table (upstream `getLanguageFromPath`)
// ---------------------------------------------------------------------------

/// `getLanguageFromPath` (theme.ts:954-1016): the lowercase last extension
/// mapped onto the canonical highlight.js language id. Returns `None` for
/// unknown extensions, exactly like upstream (no auto-detection).
///
/// Bare `Dockerfile` / `Makefile` paths resolve through the same table
/// (`split(".").pop()` keeps the whole name when there is no dot).
pub fn language_from_path(file_path: &str) -> Option<&'static str> {
    let extension = file_path.rsplit('.').next().unwrap_or(file_path);
    let extension = extension.to_ascii_lowercase();
    if let Some(language) = language_from_extension(&extension) {
        return Some(language);
    }
    // Upstream's `split(".")` only sees a bare name; accept a directory
    // prefix too so `src/Dockerfile` is not silently plain.
    let file_name = file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_path)
        .to_ascii_lowercase();
    language_from_extension(&file_name)
}

/// The upstream extension table, exactly (theme.ts:961-1011).
fn language_from_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    })
}

/// Resolve a language name (a markdown fence info string or a canonical id)
/// onto the canonical id. Unknown names return `None`.
///
/// The fence info string can carry extra tokens (`rust title=…`); upstream
/// `cli-highlight` only accepts an exact language id, so only the first
/// token is considered.
pub fn normalize_language(language: &str) -> Option<&'static str> {
    let first = language.split_whitespace().next().unwrap_or("");
    if first.is_empty() {
        return None;
    }
    let lowered = first.to_ascii_lowercase();
    Some(match lowered.as_str() {
        "typescript" | "ts" | "tsx" => "typescript",
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "python" | "py" => "python",
        "ruby" | "rb" => "ruby",
        "rust" | "rs" => "rust",
        "go" | "golang" => "go",
        "java" => "java",
        "kotlin" | "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "c++" | "cc" | "cxx" | "hpp" => "cpp",
        "csharp" | "c#" | "cs" => "csharp",
        "php" => "php",
        "bash" | "sh" | "zsh" | "shell" => "bash",
        "fish" => "fish",
        "powershell" | "ps1" | "pwsh" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "markdown" | "md" => "markdown",
        "dockerfile" | "docker" => "dockerfile",
        "makefile" | "make" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" | "pl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clojure" | "clj" => "clojure",
        "elixir" | "ex" | "exs" => "elixir",
        "erlang" | "erl" => "erlang",
        "haskell" | "hs" => "haskell",
        "ocaml" | "ml" => "ocaml",
        "vim" => "vim",
        "graphql" | "gql" => "graphql",
        "protobuf" | "proto" => "protobuf",
        "hcl" | "tf" | "terraform" => "hcl",
        _ => return None,
    })
}

/// Whether a language id/alias is known to the highlighter.
pub fn supports_language(language: &str) -> bool {
    normalize_language(language).is_some()
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// `highlightCode` (theme.ts:918-941) against the global theme, with the
/// upstream fallback: an unknown/missing language keeps every line in the
/// `mdCodeBlock` color instead of guessing.
pub fn highlight_code(code: &str, language: Option<&str>) -> Vec<String> {
    with_theme(|theme| highlight_lines_with_theme(theme, code, language, ThemeColor::MdCodeBlock))
}

/// Highlight `code` with `theme`; each returned element is one output line
/// (byte-identical to the input lines when stripped of ANSI).
///
/// `fallback` styles every line when no language resolves. When a language
/// resolves but the body exceeds the highlighting cap, the tail lines are
/// returned verbatim.
pub fn highlight_lines_with_theme(
    theme: &Theme,
    code: &str,
    language: Option<&str>,
    fallback: ThemeColor,
) -> Vec<String> {
    let language = resolve_language(language);
    memoized_highlight_lines(theme, code, language, fallback, 0)
}

/// [`highlight_lines_with_theme`], but only the lines from `from_line` on.
///
/// The memoized body is still keyed by the WHOLE code (so a stateful scanner
/// resumes at the right block-comment/triple-string/template state); only the
/// requested suffix is cloned out. The incremental markdown renderer uses this
/// to paint the one mutable tail line of an open fence without re-styling the
/// lines already folded into its cached prefix.
#[doc(hidden)]
pub fn highlight_lines_from_with_theme(
    theme: &Theme,
    code: &str,
    language: Option<&str>,
    fallback: ThemeColor,
    from_line: usize,
) -> Vec<String> {
    let language = resolve_language(language);
    memoized_highlight_lines(theme, code, language, fallback, from_line)
}

fn resolve_language(language: Option<&str>) -> Option<Language> {
    language
        .and_then(normalize_language)
        .and_then(Language::from_name)
}

// ---------------------------------------------------------------------------
// Highlight memo
// ---------------------------------------------------------------------------

/// Memoized highlight bodies held at once (bounded, oldest evicted first).
const HIGHLIGHT_MEMO_ENTRIES: usize = 16;
/// Approximate byte budget of the memo (input + styled lines).
const HIGHLIGHT_MEMO_BYTES: usize = 8 * 1024 * 1024;
/// [`extends_code`] probes this many leading and trailing bytes, so a growing
/// body is recognized in O(1) instead of comparing the whole prefix.
const HIGHLIGHT_GROW_PROBE: usize = 64;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// One memoized `code` body: the styled lines plus the scanner state right
/// before the last (possibly partial) line, so an appended body re-highlights
/// only that last line and the appended lines.
struct HighlightMemoEntry {
    language: Option<Language>,
    fallback: ThemeColor,
    theme_generation: u64,
    /// Content hash of `code` (extended incrementally as the body grows).
    hash: u64,
    code: String,
    /// `code.split('\n')` styled, one element per line.
    lines: Vec<String>,
    /// Scanner state after `lines[..line_index_before_last]`.
    state_before_last: State,
    /// Cumulative input bytes (line lengths + one newline each) before the
    /// last line, for the highlighting cap.
    bytes_before_last: usize,
    /// Index of the last line (`lines.len() - 1`).
    line_index_before_last: usize,
    /// Approximate memory this entry holds.
    bytes: usize,
}

struct HighlightMemo {
    entries: VecDeque<HighlightMemoEntry>,
    total_bytes: usize,
}

static HIGHLIGHT_MEMO: Mutex<HighlightMemo> = Mutex::new(HighlightMemo {
    entries: VecDeque::new(),
    total_bytes: 0,
});

#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HighlightMemoWork {
    /// Bodies answered from the memo (exact body or append extension).
    pub hits: u64,
    /// Bodies highlighted from scratch.
    pub misses: u64,
    /// Lines whose scanner/style work ran in this process.
    pub lines_highlighted: u64,
    /// Input bytes whose scanner/style work ran in this process.
    pub bytes_highlighted: u64,
}

static HIGHLIGHT_MEMO_HITS: AtomicU64 = AtomicU64::new(0);
static HIGHLIGHT_MEMO_MISSES: AtomicU64 = AtomicU64::new(0);
static HIGHLIGHT_MEMO_LINES: AtomicU64 = AtomicU64::new(0);
static HIGHLIGHT_MEMO_BYTES_WORK: AtomicU64 = AtomicU64::new(0);

/// The highlight-memo counters as of now (tests / probes).
#[doc(hidden)]
pub fn highlight_memo_work() -> HighlightMemoWork {
    HighlightMemoWork {
        hits: HIGHLIGHT_MEMO_HITS.load(Ordering::Relaxed),
        misses: HIGHLIGHT_MEMO_MISSES.load(Ordering::Relaxed),
        lines_highlighted: HIGHLIGHT_MEMO_LINES.load(Ordering::Relaxed),
        bytes_highlighted: HIGHLIGHT_MEMO_BYTES_WORK.load(Ordering::Relaxed),
    }
}

/// Zero the highlight-memo counters (tests read a delta around a render).
#[doc(hidden)]
pub fn reset_highlight_memo_work() {
    for counter in [
        &HIGHLIGHT_MEMO_HITS,
        &HIGHLIGHT_MEMO_MISSES,
        &HIGHLIGHT_MEMO_LINES,
        &HIGHLIGHT_MEMO_BYTES_WORK,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

fn over_highlight_cap(index: usize, bytes: usize, line: &str) -> bool {
    index >= MAX_HIGHLIGHT_LINES
        || line.len() > MAX_HIGHLIGHT_BYTES
        || (bytes > 0 && bytes.saturating_add(line.len()).saturating_add(1) > MAX_HIGHLIGHT_BYTES)
}

fn highlight_one_line(
    theme: &Theme,
    highlighter: &mut Option<Highlighter>,
    line: &str,
    fallback: ThemeColor,
    over_cap: bool,
) -> String {
    match highlighter {
        Some(highlighter) if !over_cap => highlighter.highlight_line(theme, line),
        Some(_) => line.to_string(),
        None => theme.fg(fallback, line),
    }
}

fn build_highlight_entry(
    theme: &Theme,
    code: &str,
    language: Option<Language>,
    fallback: ThemeColor,
    theme_generation: u64,
) -> HighlightMemoEntry {
    let split: Vec<&str> = code.split('\n').collect();
    let last = split.len() - 1;
    let mut highlighter = language.map(Highlighter::new);
    let mut lines = Vec::with_capacity(split.len());
    let mut bytes = 0usize;
    let mut state_before_last = State::Normal;
    let mut bytes_before_last = 0usize;
    let mut line_index_before_last = 0usize;
    for (index, line) in split.iter().enumerate() {
        if index == last {
            state_before_last = highlighter.as_ref().map_or(State::Normal, |h| h.state);
            bytes_before_last = bytes;
            line_index_before_last = index;
        }
        let over_cap = over_highlight_cap(index, bytes, line);
        lines.push(highlight_one_line(
            theme,
            &mut highlighter,
            line,
            fallback,
            over_cap,
        ));
        bytes = bytes.saturating_add(line.len()).saturating_add(1);
    }
    HIGHLIGHT_MEMO_LINES.fetch_add(split.len() as u64, Ordering::Relaxed);
    HIGHLIGHT_MEMO_BYTES_WORK.fetch_add(code.len() as u64, Ordering::Relaxed);
    let bytes = code.len() + lines.iter().map(String::len).sum::<usize>();
    HighlightMemoEntry {
        language,
        fallback,
        theme_generation,
        hash: hash_bytes(FNV_OFFSET, code.as_bytes()),
        code: code.to_string(),
        lines,
        state_before_last,
        bytes_before_last,
        line_index_before_last,
        bytes,
    }
}

/// Whether `new` is `old` plus appended bytes, probed in O(1): the live
/// callers only ever grow, but a same-length replacement is rejected here
/// (the exact-hit path owns equal lengths) and a longer replacement that
/// shares both probes would be misread as an append (never produced by the
/// stream callers).
fn extends_code(old: &str, new: &str) -> bool {
    let old_len = old.len();
    if new.len() <= old_len || !new.is_char_boundary(old_len) {
        return false;
    }
    let probe = old_len.min(HIGHLIGHT_GROW_PROBE);
    let tail_from = old_len - probe;
    old.as_bytes()[..probe] == new.as_bytes()[..probe]
        && old.as_bytes()[tail_from..] == new.as_bytes()[tail_from..old_len]
}

/// Extend `entry` to cover `new_code`, re-highlighting only the old last
/// (possibly partial) line and the appended bytes. Sets `entry.bytes` and
/// returns `(old_bytes, new_bytes)` for the memo's byte budget.
fn extend_highlight_entry(
    theme: &Theme,
    entry: &mut HighlightMemoEntry,
    new_code: &str,
) -> (usize, usize) {
    let old_bytes = entry.bytes;
    let old_code_len = entry.code.len();
    let keep = entry.code.rfind('\n').map_or(0, |index| index + 1);
    let appended = &new_code[keep..];
    let old_last_len = entry
        .lines
        .get(entry.line_index_before_last)
        .map_or(0, String::len);
    entry.lines.truncate(entry.line_index_before_last);
    let mut highlighter = entry.language.map(Highlighter::new);
    if let Some(highlighter) = highlighter.as_mut() {
        highlighter.state = entry.state_before_last;
    }
    let mut bytes = entry.bytes_before_last;
    let split: Vec<&str> = appended.split('\n').collect();
    let last_offset = split.len() - 1;
    let mut added_line_bytes = 0usize;
    for (index, (offset, line)) in (entry.line_index_before_last..).zip(split.iter().enumerate()) {
        if offset == last_offset {
            entry.state_before_last = highlighter.as_ref().map_or(State::Normal, |h| h.state);
            entry.bytes_before_last = bytes;
            entry.line_index_before_last = index;
        }
        let over_cap = over_highlight_cap(index, bytes, line);
        let rendered = highlight_one_line(theme, &mut highlighter, line, entry.fallback, over_cap);
        added_line_bytes += rendered.len();
        entry.lines.push(rendered);
        bytes = bytes.saturating_add(line.len()).saturating_add(1);
    }
    entry.code.clear();
    entry.code.push_str(new_code);
    entry.hash = hash_bytes(entry.hash, appended.as_bytes());
    entry.bytes = old_bytes
        .saturating_sub(old_last_len)
        .saturating_add(added_line_bytes)
        .saturating_add(new_code.len().saturating_sub(old_code_len));
    HIGHLIGHT_MEMO_LINES.fetch_add(split.len() as u64, Ordering::Relaxed);
    HIGHLIGHT_MEMO_BYTES_WORK.fetch_add(appended.len() as u64, Ordering::Relaxed);
    (old_bytes, entry.bytes)
}

fn slice_lines(lines: &[String], from_line: usize) -> Vec<String> {
    if from_line >= lines.len() {
        Vec::new()
    } else {
        lines[from_line..].to_vec()
    }
}

fn memoized_highlight_lines(
    theme: &Theme,
    code: &str,
    language: Option<Language>,
    fallback: ThemeColor,
    from_line: usize,
) -> Vec<String> {
    let theme_generation = Theme::generation();
    let mut memo = HIGHLIGHT_MEMO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Exact body: the steady-state re-render (spinner tick, width-stable
    // frame) pays one compare and a clone of the cached lines. The content
    // hash is computed lazily — only when a same-length candidate exists — so
    // an append-only stream never pays an O(n) hash per frame.
    let mut incoming_hash: Option<u64> = None;
    for index in (0..memo.entries.len()).rev() {
        let entry = &memo.entries[index];
        if entry.language == language
            && entry.fallback == fallback
            && entry.theme_generation == theme_generation
            && entry.code.len() == code.len()
        {
            let hash =
                *incoming_hash.get_or_insert_with(|| hash_bytes(FNV_OFFSET, code.as_bytes()));
            if entry.hash == hash && entry.code == code {
                HIGHLIGHT_MEMO_HITS.fetch_add(1, Ordering::Relaxed);
                return slice_lines(&entry.lines, from_line);
            }
        }
    }
    // Append-only growth: continue from the longest stored prefix. The probe
    // keeps this O(1); the extension re-highlights only the old last line and
    // the appended bytes.
    let mut best: Option<usize> = None;
    let mut best_len = 0usize;
    for (index, entry) in memo.entries.iter().enumerate() {
        if entry.language == language
            && entry.fallback == fallback
            && entry.theme_generation == theme_generation
            && !entry.code.is_empty()
            && entry.code.len() >= best_len
            && extends_code(&entry.code, code)
        {
            best = Some(index);
            best_len = entry.code.len();
        }
    }
    if let Some(index) = best {
        HIGHLIGHT_MEMO_HITS.fetch_add(1, Ordering::Relaxed);
        let (old_bytes, new_bytes, lines) = {
            let entry = &mut memo.entries[index];
            let (old_bytes, new_bytes) = extend_highlight_entry(theme, entry, code);
            let lines = slice_lines(&entry.lines, from_line);
            (old_bytes, new_bytes, lines)
        };
        memo.total_bytes = memo.total_bytes.saturating_sub(old_bytes) + new_bytes;
        while memo.total_bytes > HIGHLIGHT_MEMO_BYTES && memo.entries.len() > 1 {
            let Some(removed) = memo.entries.pop_front() else {
                break;
            };
            memo.total_bytes = memo.total_bytes.saturating_sub(removed.bytes);
        }
        return lines;
    }
    HIGHLIGHT_MEMO_MISSES.fetch_add(1, Ordering::Relaxed);
    let entry = build_highlight_entry(theme, code, language, fallback, theme_generation);
    let bytes = entry.bytes;
    memo.entries.push_back(entry);
    memo.total_bytes = memo.total_bytes.saturating_add(bytes);
    while memo.entries.len() > HIGHLIGHT_MEMO_ENTRIES
        || (memo.total_bytes > HIGHLIGHT_MEMO_BYTES && memo.entries.len() > 1)
    {
        let Some(removed) = memo.entries.pop_front() else {
            break;
        };
        memo.total_bytes = memo.total_bytes.saturating_sub(removed.bytes);
    }
    let index = memo.entries.len().saturating_sub(1);
    memo.entries
        .get(index)
        .map_or_else(Vec::new, |entry| slice_lines(&entry.lines, from_line))
}

// ---------------------------------------------------------------------------
// Language ids and per-language grammar knobs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    JavaScript,
    TypeScript,
    Python,
    Ruby,
    Rust,
    Go,
    Java,
    Kotlin,
    Swift,
    C,
    Cpp,
    CSharp,
    Php,
    Bash,
    Fish,
    PowerShell,
    Sql,
    Html,
    Css,
    Scss,
    Sass,
    Less,
    Json,
    Yaml,
    Toml,
    Xml,
    Markdown,
    Dockerfile,
    Makefile,
    CMake,
    Lua,
    Perl,
    R,
    Scala,
    Clojure,
    Elixir,
    Erlang,
    Haskell,
    OCaml,
    Vim,
    GraphQL,
    Protobuf,
    Hcl,
}

impl Language {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "typescript" => Language::TypeScript,
            "javascript" => Language::JavaScript,
            "python" => Language::Python,
            "ruby" => Language::Ruby,
            "rust" => Language::Rust,
            "go" => Language::Go,
            "java" => Language::Java,
            "kotlin" => Language::Kotlin,
            "swift" => Language::Swift,
            "c" => Language::C,
            "cpp" => Language::Cpp,
            "csharp" => Language::CSharp,
            "php" => Language::Php,
            "bash" => Language::Bash,
            "fish" => Language::Fish,
            "powershell" => Language::PowerShell,
            "sql" => Language::Sql,
            "html" => Language::Html,
            "css" => Language::Css,
            "scss" => Language::Scss,
            "sass" => Language::Sass,
            "less" => Language::Less,
            "json" => Language::Json,
            "yaml" => Language::Yaml,
            "toml" => Language::Toml,
            "xml" => Language::Xml,
            "markdown" => Language::Markdown,
            "dockerfile" => Language::Dockerfile,
            "makefile" => Language::Makefile,
            "cmake" => Language::CMake,
            "lua" => Language::Lua,
            "perl" => Language::Perl,
            "r" => Language::R,
            "scala" => Language::Scala,
            "clojure" => Language::Clojure,
            "elixir" => Language::Elixir,
            "erlang" => Language::Erlang,
            "haskell" => Language::Haskell,
            "ocaml" => Language::OCaml,
            "vim" => Language::Vim,
            "graphql" => Language::GraphQL,
            "protobuf" => Language::Protobuf,
            "hcl" => Language::Hcl,
            _ => return None,
        })
    }

    /// Line-comment starts, checked before operators/strings.
    fn line_comments(self) -> &'static [&'static str] {
        match self {
            Language::Python
            | Language::Ruby
            | Language::Bash
            | Language::Fish
            | Language::PowerShell
            | Language::Yaml
            | Language::Toml
            | Language::Dockerfile
            | Language::Makefile
            | Language::CMake
            | Language::Perl
            | Language::R
            | Language::Elixir
            | Language::Hcl
            | Language::GraphQL => &["#"],
            Language::Sql | Language::Lua | Language::Haskell => &["--"],
            Language::Clojure => &[";"],
            Language::Erlang => &["%"],
            Language::Vim => &["\""],
            Language::Json | Language::Markdown => &[],
            _ => &["//"],
        }
    }

    /// Block-comment delimiter pair `(open, close, nested)`.
    fn block_comment(self) -> Option<(&'static str, &'static str, bool)> {
        match self {
            Language::Haskell => Some(("{-", "-}", true)),
            Language::OCaml => Some(("(*", "*)", true)),
            Language::Lua => Some(("--[[", "]]", false)),
            Language::Html | Language::Xml => Some(("<!--", "-->", false)),
            Language::PowerShell => Some(("<#", "#>", false)),
            Language::Json
            | Language::Yaml
            | Language::Toml
            | Language::Python
            | Language::Ruby
            | Language::Markdown
            | Language::Dockerfile
            | Language::Makefile
            | Language::Clojure
            | Language::Elixir
            | Language::Erlang
            | Language::Vim => None,
            _ => Some(("/*", "*/", true)),
        }
    }

    /// Quote characters that start a string literal on this line.
    fn string_quotes(self) -> &'static [char] {
        match self {
            Language::JavaScript
            | Language::TypeScript
            | Language::Go
            | Language::Bash
            | Language::Fish => &['\'', '"', '`'],
            Language::Rust => &['"'],
            Language::Vim => &['\''],
            Language::Markdown => &['`'],
            _ => &['\'', '"'],
        }
    }

    /// Backtick strings may continue on the next line (JS template /
    /// Go raw string).
    fn backtick_is_multiline(self) -> bool {
        matches!(
            self,
            Language::JavaScript
                | Language::TypeScript
                | Language::Go
                | Language::Bash
                | Language::Fish
        )
    }

    fn case_insensitive_keywords(self) -> bool {
        matches!(
            self,
            Language::Sql | Language::PowerShell | Language::Dockerfile | Language::CMake
        )
    }

    /// Character that turns an identifier/string on its left into a key.
    fn key_separator(self) -> Option<char> {
        match self {
            Language::Json | Language::Yaml => Some(':'),
            Language::Toml => Some('='),
            Language::Css | Language::Scss | Language::Sass | Language::Less => Some(':'),
            _ => None,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Language::JavaScript => JS_KEYWORDS,
            Language::TypeScript => TS_KEYWORDS,
            Language::Python => PYTHON_KEYWORDS,
            Language::Ruby => RUBY_KEYWORDS,
            Language::Rust => RUST_KEYWORDS,
            Language::Go => GO_KEYWORDS,
            Language::Java => JAVA_KEYWORDS,
            Language::Kotlin => KOTLIN_KEYWORDS,
            Language::Swift => SWIFT_KEYWORDS,
            Language::C => C_KEYWORDS,
            Language::Cpp => CPP_KEYWORDS,
            Language::CSharp => CSHARP_KEYWORDS,
            Language::Php => PHP_KEYWORDS,
            Language::Bash | Language::Fish => SHELL_KEYWORDS,
            Language::PowerShell => POWERSHELL_KEYWORDS,
            Language::Sql => SQL_KEYWORDS,
            Language::Css | Language::Scss | Language::Sass | Language::Less => CSS_KEYWORDS,
            Language::Json | Language::Html | Language::Xml | Language::Markdown => &[],
            Language::Yaml => YAML_KEYWORDS,
            Language::Toml => TOML_KEYWORDS,
            Language::Dockerfile => DOCKERFILE_KEYWORDS,
            Language::Makefile => MAKE_KEYWORDS,
            Language::CMake => CMAKE_KEYWORDS,
            Language::Lua => LUA_KEYWORDS,
            Language::Perl => PERL_KEYWORDS,
            Language::R => R_KEYWORDS,
            Language::Scala => SCALA_KEYWORDS,
            Language::Clojure => CLOJURE_KEYWORDS,
            Language::Elixir => ELIXIR_KEYWORDS,
            Language::Erlang => ERLANG_KEYWORDS,
            Language::Haskell => HASKELL_KEYWORDS,
            Language::OCaml => OCAML_KEYWORDS,
            Language::Vim => VIM_KEYWORDS,
            Language::GraphQL => GRAPHQL_KEYWORDS,
            Language::Protobuf => PROTOBUF_KEYWORDS,
            Language::Hcl => HCL_KEYWORDS,
        }
    }

    fn types(self) -> &'static [&'static str] {
        match self {
            Language::JavaScript => JS_TYPES,
            Language::TypeScript => TS_TYPES,
            Language::Python => PYTHON_TYPES,
            Language::Rust => RUST_TYPES,
            Language::Go => GO_TYPES,
            Language::Java => JAVA_TYPES,
            Language::Kotlin => KOTLIN_TYPES,
            Language::Swift => SWIFT_TYPES,
            Language::C => C_TYPES,
            Language::Cpp => CPP_TYPES,
            Language::CSharp => CSHARP_TYPES,
            Language::Php => PHP_TYPES,
            Language::Bash | Language::Fish => SHELL_BUILTINS,
            Language::PowerShell => POWERSHELL_TYPES,
            Language::Sql => SQL_TYPES,
            Language::Protobuf => PROTOBUF_TYPES,
            Language::Hcl => HCL_TYPES,
            _ => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Keyword tables
// ---------------------------------------------------------------------------

const JS_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];
const JS_TYPES: &[&str] = &[
    "Array", "Boolean", "Date", "Error", "JSON", "Map", "Math", "Number", "Object", "Promise",
    "RegExp", "Set", "String", "Symbol", "WeakMap", "WeakSet",
];
const TS_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "infer",
    "instanceof",
    "interface",
    "is",
    "keyof",
    "let",
    "namespace",
    "new",
    "of",
    "override",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "satisfies",
    "set",
    "static",
    "super",
    "switch",
    "throw",
    "try",
    "type",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];
const TS_TYPES: &[&str] = &[
    "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
    "Array", "Boolean", "Date", "Error", "JSON", "Map", "Math", "Number", "Object", "Promise",
    "RegExp", "Set", "String", "Symbol",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];
const PYTHON_TYPES: &[&str] = &[
    "bool",
    "bytes",
    "bytearray",
    "complex",
    "dict",
    "float",
    "frozenset",
    "int",
    "list",
    "memoryview",
    "object",
    "range",
    "set",
    "str",
    "tuple",
    "type",
];
const RUBY_KEYWORDS: &[&str] = &[
    "alias", "begin", "break", "case", "class", "def", "defined", "do", "else", "elsif", "end",
    "ensure", "for", "if", "in", "module", "next", "nil", "not", "or", "redo", "rescue", "retry",
    "return", "self", "super", "then", "undef", "unless", "until", "when", "while", "yield",
];
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const RUST_TYPES: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8", "u16",
    "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result", "Box", "Rc", "Arc",
    "HashMap", "BTreeMap", "HashSet", "BTreeSet", "Cell", "RefCell", "Mutex", "RwLock",
];
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];
const GO_TYPES: &[&str] = &[
    "bool",
    "byte",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "any",
];
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "interface",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "volatile",
    "while",
];
const JAVA_TYPES: &[&str] = &[
    "boolean", "byte", "char", "double", "float", "int", "long", "short", "void", "String",
    "Object", "Integer", "Long", "Double", "Boolean", "List", "Map", "Set",
];
const KOTLIN_KEYWORDS: &[&str] = &[
    "as",
    "as?",
    "break",
    "by",
    "catch",
    "class",
    "companion",
    "const",
    "constructor",
    "continue",
    "crossinline",
    "data",
    "delegate",
    "do",
    "dynamic",
    "else",
    "enum",
    "expect",
    "external",
    "field",
    "file",
    "final",
    "finally",
    "for",
    "fun",
    "get",
    "if",
    "import",
    "in",
    "infix",
    "init",
    "inline",
    "inner",
    "interface",
    "internal",
    "is",
    "it",
    "lateinit",
    "noinline",
    "object",
    "open",
    "operator",
    "out",
    "override",
    "package",
    "param",
    "private",
    "protected",
    "public",
    "reified",
    "return",
    "sealed",
    "set",
    "super",
    "suspend",
    "tailrec",
    "this",
    "throw",
    "try",
    "typealias",
    "typeof",
    "val",
    "var",
    "vararg",
    "when",
    "where",
    "while",
];
const KOTLIN_TYPES: &[&str] = &[
    "Any", "Array", "Boolean", "Byte", "Char", "Double", "Float", "Int", "Long", "Nothing",
    "Short", "String", "Unit", "List", "Map", "Set",
];
const SWIFT_KEYWORDS: &[&str] = &[
    "as",
    "associatedtype",
    "break",
    "case",
    "catch",
    "class",
    "continue",
    "default",
    "defer",
    "deinit",
    "do",
    "else",
    "enum",
    "extension",
    "fallthrough",
    "fileprivate",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "indirect",
    "init",
    "inout",
    "internal",
    "is",
    "lazy",
    "let",
    "mutating",
    "nonmutating",
    "open",
    "operator",
    "override",
    "private",
    "protocol",
    "public",
    "repeat",
    "required",
    "rethrows",
    "return",
    "static",
    "struct",
    "subscript",
    "super",
    "switch",
    "throw",
    "throws",
    "try",
    "typealias",
    "var",
    "where",
    "while",
];
const SWIFT_TYPES: &[&str] = &[
    "Any",
    "AnyObject",
    "Array",
    "Bool",
    "Character",
    "Dictionary",
    "Double",
    "Error",
    "Float",
    "Int",
    "Optional",
    "Set",
    "String",
    "UInt",
    "Void",
];
const C_KEYWORDS: &[&str] = &[
    "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for",
    "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static", "struct",
    "switch", "typedef", "union", "volatile", "while", "define", "include", "ifdef", "ifndef",
    "endif", "pragma",
];
const C_TYPES: &[&str] = &[
    "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void", "_Bool",
    "size_t", "ssize_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t",
    "uint32_t", "uint64_t", "FILE",
];
const CPP_KEYWORDS: &[&str] = &[
    "alignas",
    "alignof",
    "auto",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "constexpr",
    "continue",
    "decltype",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "explicit",
    "export",
    "extern",
    "final",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "return",
    "sizeof",
    "static",
    "static_assert",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "try",
    "typedef",
    "typename",
    "union",
    "using",
    "virtual",
    "volatile",
    "while",
    "define",
    "include",
    "ifdef",
    "ifndef",
    "endif",
    "pragma",
];
const CPP_TYPES: &[&str] = &[
    "bool",
    "char",
    "double",
    "float",
    "int",
    "long",
    "short",
    "signed",
    "unsigned",
    "void",
    "wchar_t",
    "size_t",
    "string",
    "vector",
    "map",
    "set",
    "unique_ptr",
    "shared_ptr",
];
const CSHARP_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "base",
    "break",
    "case",
    "catch",
    "checked",
    "class",
    "const",
    "continue",
    "default",
    "delegate",
    "do",
    "else",
    "enum",
    "event",
    "explicit",
    "extern",
    "finally",
    "fixed",
    "for",
    "foreach",
    "goto",
    "if",
    "implicit",
    "in",
    "interface",
    "internal",
    "is",
    "lock",
    "namespace",
    "new",
    "operator",
    "out",
    "override",
    "params",
    "private",
    "protected",
    "public",
    "readonly",
    "ref",
    "return",
    "sealed",
    "sizeof",
    "stackalloc",
    "static",
    "struct",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "unchecked",
    "unsafe",
    "using",
    "virtual",
    "volatile",
    "while",
    "yield",
];
const CSHARP_TYPES: &[&str] = &[
    "bool", "byte", "char", "decimal", "double", "float", "int", "long", "object", "sbyte",
    "short", "string", "uint", "ulong", "ushort", "void", "var", "dynamic",
];
const PHP_KEYWORDS: &[&str] = &[
    "abstract",
    "and",
    "array",
    "as",
    "break",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "do",
    "echo",
    "else",
    "elseif",
    "empty",
    "endfor",
    "endforeach",
    "endif",
    "endswitch",
    "endwhile",
    "extends",
    "final",
    "finally",
    "fn",
    "for",
    "foreach",
    "function",
    "global",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "interface",
    "isset",
    "list",
    "match",
    "namespace",
    "new",
    "or",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "static",
    "switch",
    "throw",
    "trait",
    "try",
    "unset",
    "use",
    "var",
    "while",
    "xor",
    "yield",
];
const PHP_TYPES: &[&str] = &[
    "bool", "float", "int", "iterable", "mixed", "never", "object", "string", "void", "null",
];
const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "in", "return", "local", "export", "readonly", "declare", "typeset", "unset",
    "shift", "break", "continue", "select", "time", "coproc",
];
const SHELL_BUILTINS: &[&str] = &[
    "alias", "bg", "cd", "command", "echo", "eval", "exec", "exit", "fg", "getopts", "hash",
    "jobs", "kill", "printf", "pwd", "read", "set", "source", "test", "trap", "type", "ulimit",
    "umask", "wait",
];
const POWERSHELL_KEYWORDS: &[&str] = &[
    "begin",
    "break",
    "catch",
    "class",
    "continue",
    "data",
    "define",
    "do",
    "dynamicparam",
    "else",
    "elseif",
    "end",
    "enum",
    "exit",
    "filter",
    "finally",
    "for",
    "foreach",
    "from",
    "function",
    "if",
    "in",
    "param",
    "process",
    "return",
    "switch",
    "throw",
    "trap",
    "try",
    "until",
    "using",
    "while",
    "workflow",
];
const POWERSHELL_TYPES: &[&str] = &[
    "Get-ChildItem",
    "Get-Content",
    "Get-Item",
    "Get-Process",
    "Set-Content",
    "Set-Item",
    "Write-Host",
    "Write-Output",
    "Where-Object",
    "ForEach-Object",
    "Select-Object",
];
const SQL_KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "any",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "case",
    "check",
    "column",
    "commit",
    "constraint",
    "create",
    "database",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "foreign",
    "from",
    "full",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "into",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "right",
    "rollback",
    "select",
    "set",
    "table",
    "then",
    "union",
    "unique",
    "update",
    "values",
    "view",
    "when",
    "where",
    "with",
];
const SQL_TYPES: &[&str] = &[
    "bigint",
    "bit",
    "blob",
    "boolean",
    "char",
    "date",
    "datetime",
    "decimal",
    "double",
    "float",
    "int",
    "integer",
    "json",
    "numeric",
    "real",
    "smallint",
    "text",
    "time",
    "timestamp",
    "tinyint",
    "varchar",
];
const CSS_KEYWORDS: &[&str] = &[
    "@charset",
    "@font-face",
    "@import",
    "@keyframes",
    "@media",
    "@page",
    "@supports",
    "auto",
    "block",
    "border-box",
    "content-box",
    "flex",
    "grid",
    "important",
    "inherit",
    "initial",
    "inline",
    "inline-block",
    "none",
    "unset",
];
const YAML_KEYWORDS: &[&str] = &["on", "off", "yes", "no"];
const TOML_KEYWORDS: &[&str] = &[];
const DOCKERFILE_KEYWORDS: &[&str] = &[
    "add",
    "arg",
    "cmd",
    "copy",
    "entrypoint",
    "env",
    "expose",
    "from",
    "healthcheck",
    "label",
    "maintainer",
    "onbuild",
    "run",
    "shell",
    "stopsignal",
    "user",
    "volume",
    "workdir",
];
const MAKE_KEYWORDS: &[&str] = &[
    "define", "else", "endef", "endif", "export", "ifdef", "ifeq", "ifndef", "ifneq", "include",
    "override", "unexport", "vpath",
];
const CMAKE_KEYWORDS: &[&str] = &[
    "break",
    "continue",
    "else",
    "elseif",
    "endforeach",
    "endfunction",
    "endif",
    "endmacro",
    "endwhile",
    "foreach",
    "function",
    "if",
    "macro",
    "return",
    "while",
];
const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in", "local",
    "not", "or", "repeat", "return", "then", "until", "while",
];
const PERL_KEYWORDS: &[&str] = &[
    "bless", "die", "do", "else", "elsif", "eval", "for", "foreach", "given", "goto", "if", "last",
    "local", "my", "next", "our", "package", "print", "redo", "require", "return", "say", "state",
    "sub", "unless", "until", "use", "when", "while",
];
const R_KEYWORDS: &[&str] = &[
    "break", "else", "for", "function", "if", "in", "next", "repeat", "return", "while",
];
const SCALA_KEYWORDS: &[&str] = &[
    "abstract",
    "case",
    "catch",
    "class",
    "def",
    "do",
    "else",
    "extends",
    "false",
    "final",
    "finally",
    "for",
    "forSome",
    "if",
    "implicit",
    "import",
    "lazy",
    "match",
    "new",
    "null",
    "object",
    "override",
    "package",
    "private",
    "protected",
    "return",
    "sealed",
    "super",
    "this",
    "throw",
    "trait",
    "try",
    "true",
    "type",
    "val",
    "var",
    "while",
    "with",
    "yield",
];
const CLOJURE_KEYWORDS: &[&str] = &[
    "catch",
    "def",
    "defn",
    "defmacro",
    "defmethod",
    "defmulti",
    "defn-",
    "defonce",
    "defprotocol",
    "defrecord",
    "defstruct",
    "do",
    "finally",
    "fn",
    "for",
    "if",
    "import",
    "let",
    "loop",
    "ns",
    "quote",
    "recur",
    "require",
    "set!",
    "throw",
    "try",
    "use",
    "var",
    "when",
    "while",
];
const ELIXIR_KEYWORDS: &[&str] = &[
    "after",
    "alias",
    "and",
    "case",
    "catch",
    "cond",
    "def",
    "defmacro",
    "defmodule",
    "defp",
    "defprotocol",
    "defstruct",
    "do",
    "else",
    "end",
    "fn",
    "for",
    "if",
    "import",
    "in",
    "not",
    "or",
    "raise",
    "receive",
    "require",
    "rescue",
    "try",
    "unless",
    "use",
    "when",
];
const ERLANG_KEYWORDS: &[&str] = &[
    "after", "and", "andalso", "begin", "case", "catch", "cond", "end", "fun", "if", "let", "of",
    "or", "orelse", "receive", "try", "when", "xor",
];
const HASKELL_KEYWORDS: &[&str] = &[
    "as",
    "case",
    "class",
    "data",
    "default",
    "deriving",
    "do",
    "else",
    "foreign",
    "hiding",
    "if",
    "import",
    "in",
    "infix",
    "infixl",
    "infixr",
    "instance",
    "let",
    "module",
    "newtype",
    "of",
    "qualified",
    "then",
    "type",
    "where",
];
const OCAML_KEYWORDS: &[&str] = &[
    "and",
    "as",
    "begin",
    "class",
    "constraint",
    "do",
    "done",
    "downto",
    "else",
    "end",
    "exception",
    "external",
    "false",
    "for",
    "fun",
    "function",
    "functor",
    "if",
    "in",
    "include",
    "inherit",
    "initializer",
    "lazy",
    "let",
    "match",
    "method",
    "module",
    "mutable",
    "new",
    "object",
    "of",
    "open",
    "or",
    "private",
    "rec",
    "sig",
    "struct",
    "then",
    "to",
    "true",
    "try",
    "type",
    "val",
    "virtual",
    "when",
    "while",
    "with",
];
const VIM_KEYWORDS: &[&str] = &[
    "autocmd",
    "augroup",
    "call",
    "command",
    "endif",
    "endfor",
    "endfunction",
    "endwhile",
    "else",
    "elseif",
    "for",
    "function",
    "if",
    "in",
    "let",
    "map",
    "nnoremap",
    "noremap",
    "return",
    "set",
    "silent",
    "unlet",
    "while",
];
const GRAPHQL_KEYWORDS: &[&str] = &[
    "directive",
    "enum",
    "extend",
    "fragment",
    "implements",
    "input",
    "interface",
    "mutation",
    "on",
    "query",
    "scalar",
    "schema",
    "subscription",
    "type",
    "union",
];
const PROTOBUF_KEYWORDS: &[&str] = &[
    "enum",
    "extend",
    "extensions",
    "import",
    "map",
    "message",
    "oneof",
    "option",
    "optional",
    "package",
    "repeated",
    "required",
    "reserved",
    "returns",
    "rpc",
    "service",
    "stream",
    "syntax",
    "weak",
];
const PROTOBUF_TYPES: &[&str] = &[
    "bool", "bytes", "double", "fixed32", "fixed64", "float", "int32", "int64", "sfixed32",
    "sfixed64", "sint32", "sint64", "string", "uint32", "uint64",
];
const HCL_KEYWORDS: &[&str] = &[
    "data",
    "dynamic",
    "for",
    "if",
    "in",
    "locals",
    "module",
    "output",
    "provider",
    "resource",
    "terraform",
    "variable",
];
const HCL_TYPES: &[&str] = &[
    "bool", "list", "map", "number", "object", "set", "string", "tuple",
];

/// Literals styled like upstream's `literal` scope (`syntaxNumber`).
fn is_literal_word(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "nil" | "undefined" | "none" | "nan" | "infinity" | "inf"
    )
}

// ---------------------------------------------------------------------------
// Scanner state
// ---------------------------------------------------------------------------

type Span = (usize, usize, ThemeColor);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Normal,
    BlockComment {
        opener: &'static str,
        close: &'static str,
        nested: bool,
        depth: u32,
    },
    /// Python `"""…"""` / `'''…'''`.
    TripleString {
        quote: char,
    },
    /// JS template literal / Go raw string / shell backtick.
    TemplateString,
    HtmlTag {
        name_pending: bool,
    },
}

struct Highlighter {
    language: Language,
    state: State,
}

impl Highlighter {
    fn new(language: Language) -> Self {
        Self {
            language,
            state: State::Normal,
        }
    }

    /// Highlight one line (no trailing newline). State carries into the next
    /// call, so block comments/triple strings/template literals span lines.
    fn highlight_line(&mut self, theme: &Theme, line: &str) -> String {
        let spans = self.scan_line(line);
        if spans.is_empty() {
            return line.to_string();
        }
        let mut out = String::with_capacity(line.len() + spans.len() * 8);
        let mut cursor = 0usize;
        for (start, end, color) in spans {
            if start > cursor {
                out.push_str(&line[cursor..start]);
            }
            out.push_str(&theme.fg(color, &line[start..end]));
            cursor = end;
        }
        if cursor < line.len() {
            out.push_str(&line[cursor..]);
        }
        out
    }

    fn scan_line(&mut self, line: &str) -> Vec<Span> {
        // A markdown heading / fence is one keyword-colored line; inline
        // spans are not part of the chat surface's needs.
        if self.language == Language::Markdown && self.state == State::Normal {
            let trimmed = line.trim_start();
            if !trimmed.is_empty()
                && (trimmed.starts_with('#')
                    || trimmed.starts_with("```")
                    || trimmed.starts_with("~~~"))
            {
                return vec![(0, line.len(), ThemeColor::SyntaxKeyword)];
            }
        }

        let mut spans: Vec<Span> = Vec::new();
        let mut pos = 0usize;
        let mut last_significant: Option<char> = None;
        let mut last_word: Option<&str> = None;

        loop {
            // Consume a state carried over from the previous line (or opened
            // earlier on this one).
            match self.state {
                State::BlockComment {
                    opener,
                    close,
                    nested,
                    mut depth,
                } => {
                    let start = pos;
                    pos = consume_block_comment(line, pos, opener, close, nested, &mut depth);
                    push_span(&mut spans, start, pos, ThemeColor::SyntaxComment);
                    if depth == 0 {
                        self.state = State::Normal;
                        continue;
                    }
                    self.state = State::BlockComment {
                        opener,
                        close,
                        nested,
                        depth,
                    };
                    break;
                }
                State::TripleString { quote } => {
                    let delimiter = triple_delimiter(quote);
                    let start = pos;
                    let (end, closed) = consume_multiline_string(line, pos, delimiter);
                    push_span(&mut spans, start, end, ThemeColor::SyntaxString);
                    if closed {
                        self.state = State::Normal;
                        pos = end;
                        continue;
                    }
                    break;
                }
                State::TemplateString => {
                    let start = pos;
                    let (end, closed) = consume_multiline_string(line, pos, "`");
                    push_span(&mut spans, start, end, ThemeColor::SyntaxString);
                    if closed {
                        self.state = State::Normal;
                        pos = end;
                        continue;
                    }
                    break;
                }
                State::HtmlTag { .. } => {
                    pos = self.scan_html_tag(line, pos, &mut spans);
                    if matches!(self.state, State::HtmlTag { .. }) {
                        break;
                    }
                    continue;
                }
                State::Normal => {}
            }

            if pos >= line.len() {
                break;
            }
            let rest = &line[pos..];
            let Some(ch) = rest.chars().next() else {
                break;
            };
            let ch_len = ch.len_utf8();

            if ch.is_whitespace() {
                pos += ch_len;
                continue;
            }

            // Line comments (before operators/strings: `--`, `#`, `"` for vim).
            if self
                .language
                .line_comments()
                .iter()
                .any(|marker| rest.starts_with(marker))
            {
                push_span(&mut spans, pos, line.len(), ThemeColor::SyntaxComment);
                break;
            }

            // Block comments.
            if let Some((opener, close, nested)) = self.language.block_comment() {
                if rest.starts_with(opener) {
                    let start = pos;
                    let mut depth = 1u32;
                    pos = consume_block_comment(
                        line,
                        pos + opener.len(),
                        opener,
                        close,
                        nested,
                        &mut depth,
                    );
                    push_span(&mut spans, start, pos, ThemeColor::SyntaxComment);
                    if depth == 0 {
                        continue;
                    }
                    self.state = State::BlockComment {
                        opener,
                        close,
                        nested,
                        depth,
                    };
                    break;
                }
            }

            // Python triple-quoted strings.
            if self.language == Language::Python
                && (rest.starts_with("\"\"\"") || rest.starts_with("'''"))
            {
                let start = pos;
                let (end, closed) = consume_multiline_string(line, pos + 3, &rest[..3]);
                push_span(&mut spans, start, end, ThemeColor::SyntaxString);
                if closed {
                    pos = end;
                    last_significant = Some('"');
                    last_word = None;
                    continue;
                }
                self.state = State::TripleString { quote: ch };
                break;
            }

            // Quoted strings, including JS template literals.
            if self.language.string_quotes().contains(&ch) {
                let start = pos;
                if ch == '`' && self.language.backtick_is_multiline() {
                    if let Some(end) = scan_quoted(line, pos, '`') {
                        push_span(&mut spans, start, end, ThemeColor::SyntaxString);
                        pos = end;
                        last_significant = Some('`');
                        last_word = None;
                        continue;
                    }
                    push_span(&mut spans, start, line.len(), ThemeColor::SyntaxString);
                    self.state = State::TemplateString;
                    break;
                }
                let end = scan_quoted(line, pos, ch).unwrap_or(line.len());
                // JSON/YAML/TOML/CSS quoted keys are `attr` upstream, not
                // plain strings.
                let color = if self
                    .language
                    .key_separator()
                    .is_some_and(|separator| next_nonspace(line, end) == Some(separator))
                {
                    ThemeColor::SyntaxVariable
                } else {
                    ThemeColor::SyntaxString
                };
                push_span(&mut spans, start, end, color);
                pos = end;
                last_significant = Some(ch);
                last_word = None;
                continue;
            }

            // JS regex literals (upstream `regexp` maps to syntaxString).
            if ch == '/'
                && matches!(self.language, Language::JavaScript | Language::TypeScript)
                && regex_allowed(last_significant, last_word)
            {
                if let Some(end) = scan_regex(line, pos) {
                    push_span(&mut spans, pos, end, ThemeColor::SyntaxString);
                    pos = end;
                    last_significant = Some('/');
                    last_word = None;
                    continue;
                }
            }

            // HTML/XML tags.
            if matches!(self.language, Language::Html | Language::Xml) && ch == '<' {
                push_span(&mut spans, pos, pos + 1, ThemeColor::SyntaxPunctuation);
                pos += 1;
                self.state = State::HtmlTag { name_pending: true };
                continue;
            }

            // Numbers.
            if ch.is_ascii_digit()
                || (ch == '.'
                    && rest
                        .chars()
                        .nth(1)
                        .is_some_and(|next| next.is_ascii_digit()))
            {
                let end = scan_number(line, pos);
                push_span(&mut spans, pos, end, ThemeColor::SyntaxNumber);
                last_significant = line[pos..end].chars().last();
                last_word = None;
                pos = end;
                continue;
            }

            // Identifiers / keywords / types / functions / variables.
            if is_identifier_start(ch) {
                let end = identifier_end(line, pos);
                let word = &line[pos..end];
                if let Some(color) = self.classify_identifier(line, pos, end, word) {
                    push_span(&mut spans, pos, end, color);
                }
                last_significant = word.chars().last();
                last_word = Some(word);
                pos = end;
                continue;
            }

            // Operators.
            if is_operator_char(ch) {
                let mut end = pos + ch_len;
                while end < line.len() {
                    let Some(next) = line[end..].chars().next() else {
                        break;
                    };
                    if !is_operator_char(next) {
                        break;
                    }
                    end += next.len_utf8();
                }
                push_span(&mut spans, pos, end, ThemeColor::SyntaxOperator);
                last_significant = line[pos..end].chars().last();
                last_word = None;
                pos = end;
                continue;
            }

            // Remaining punctuation.
            push_span(&mut spans, pos, pos + ch_len, ThemeColor::SyntaxPunctuation);
            last_significant = Some(ch);
            last_word = None;
            pos += ch_len;
        }

        coalesce(spans)
    }

    fn classify_identifier(
        &self,
        line: &str,
        start: usize,
        end: usize,
        word: &str,
    ) -> Option<ThemeColor> {
        // Shell/PowerShell `$name` expansions are variables.
        if word.starts_with('$') {
            return Some(ThemeColor::SyntaxVariable);
        }
        if is_literal_word(word) {
            return Some(ThemeColor::SyntaxNumber);
        }
        if contains_word(
            self.language.keywords(),
            word,
            self.language.case_insensitive_keywords(),
        ) {
            return Some(ThemeColor::SyntaxKeyword);
        }
        if contains_word(
            self.language.types(),
            word,
            self.language.case_insensitive_keywords(),
        ) {
            return Some(ThemeColor::SyntaxType);
        }
        if self
            .language
            .key_separator()
            .is_some_and(|separator| next_nonspace(line, end) == Some(separator))
        {
            return Some(ThemeColor::SyntaxVariable);
        }
        if next_nonspace(line, end) == Some('(') {
            return Some(ThemeColor::SyntaxFunction);
        }
        // Upstream highlights capitalized identifiers as types (`title` /
        // `class` scopes both land on `syntaxType`).
        if word.chars().next().is_some_and(char::is_uppercase) {
            return Some(ThemeColor::SyntaxType);
        }
        if self.language == Language::Dockerfile && start == 0 {
            // Dockerfile instructions are already covered by keywords; an
            // unknown leading word is still a directive-shaped token.
            return Some(ThemeColor::SyntaxKeyword);
        }
        None
    }

    /// Scan one tag token at `pos`; leaves `state` as `HtmlTag` when the tag
    /// continues on the next line.
    fn scan_html_tag(&mut self, line: &str, mut pos: usize, spans: &mut Vec<Span>) -> usize {
        let State::HtmlTag { mut name_pending } = self.state else {
            return pos;
        };
        while pos < line.len() {
            let rest = &line[pos..];
            let Some(ch) = rest.chars().next() else {
                break;
            };
            if ch.is_whitespace() {
                pos += ch.len_utf8();
                continue;
            }
            match ch {
                '>' => {
                    push_span(spans, pos, pos + 1, ThemeColor::SyntaxPunctuation);
                    self.state = State::Normal;
                    return pos + 1;
                }
                '/' => {
                    push_span(spans, pos, pos + 1, ThemeColor::SyntaxPunctuation);
                    pos += 1;
                }
                '=' => {
                    push_span(spans, pos, pos + 1, ThemeColor::SyntaxOperator);
                    pos += 1;
                }
                '"' | '\'' => {
                    let end = scan_quoted(line, pos, ch).unwrap_or(line.len());
                    push_span(spans, pos, end, ThemeColor::SyntaxString);
                    pos = end;
                }
                '!' | '?' => {
                    push_span(spans, pos, pos + 1, ThemeColor::SyntaxPunctuation);
                    pos += 1;
                }
                _ if is_identifier_start(ch) => {
                    let end = identifier_end(line, pos);
                    let color = if name_pending {
                        name_pending = false;
                        ThemeColor::SyntaxKeyword
                    } else {
                        ThemeColor::SyntaxVariable
                    };
                    push_span(spans, pos, end, color);
                    pos = end;
                }
                _ => {
                    push_span(
                        spans,
                        pos,
                        pos + ch.len_utf8(),
                        ThemeColor::SyntaxPunctuation,
                    );
                    pos += ch.len_utf8();
                }
            }
        }
        self.state = State::HtmlTag { name_pending };
        pos
    }
}

// ---------------------------------------------------------------------------
// Scanner helpers
// ---------------------------------------------------------------------------

fn is_identifier_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_' || ch == '$'
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

fn identifier_end(line: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, ch) in line[start..].char_indices() {
        let at_start = end == start;
        if at_start {
            if !is_identifier_start(ch) {
                return start;
            }
        } else if !is_identifier_continue(ch) {
            return end;
        }
        end = start + offset + ch.len_utf8();
    }
    end
}

fn is_operator_char(ch: char) -> bool {
    matches!(
        ch,
        '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' | '!' | '&' | '|' | '^' | '~' | '?'
    )
}

fn next_nonspace(line: &str, from: usize) -> Option<char> {
    line[from..].chars().find(|ch| !ch.is_whitespace())
}

fn contains_word(words: &[&str], word: &str, case_insensitive: bool) -> bool {
    words.iter().any(|candidate| {
        if case_insensitive {
            candidate.eq_ignore_ascii_case(word)
        } else {
            *candidate == word
        }
    })
}

/// `token.text.split("\n")` semantics for a delimited multi-line string.
fn consume_multiline_string(line: &str, from: usize, delimiter: &str) -> (usize, bool) {
    let mut pos = from;
    while pos <= line.len() {
        let Some(offset) = line[pos..].find(delimiter) else {
            return (line.len(), false);
        };
        let candidate = pos + offset;
        if is_escaped(line, candidate) {
            pos = candidate + delimiter.len();
            continue;
        }
        return (candidate + delimiter.len(), true);
    }
    (line.len(), false)
}

/// Whether the byte at `index` is preceded by an odd run of backslashes.
fn is_escaped(line: &str, index: usize) -> bool {
    let mut slashes = 0usize;
    let mut cursor = index;
    while cursor > 0 {
        let Some(previous) = line[..cursor].chars().next_back() else {
            break;
        };
        if previous != '\\' {
            break;
        }
        slashes += 1;
        cursor -= previous.len_utf8();
    }
    slashes % 2 == 1
}

/// Scan a block comment from `pos`; returns the position after the closing
/// delimiter (or the line end when it stays open).
fn consume_block_comment(
    line: &str,
    mut pos: usize,
    opener: &str,
    close: &str,
    nested: bool,
    depth: &mut u32,
) -> usize {
    while pos < line.len() {
        if nested {
            let next_open = line[pos..].find(opener).map(|offset| pos + offset);
            let next_close = line[pos..].find(close).map(|offset| pos + offset);
            match (next_open, next_close) {
                (Some(open), Some(end)) if open < end => {
                    *depth += 1;
                    pos = open + opener.len();
                }
                (_, Some(end)) => {
                    *depth -= 1;
                    pos = end + close.len();
                    if *depth == 0 {
                        return pos;
                    }
                }
                (_, None) => return line.len(),
            }
        } else if let Some(offset) = line[pos..].find(close) {
            *depth = 0;
            return pos + offset + close.len();
        } else {
            return line.len();
        }
    }
    pos
}

fn scan_quoted(line: &str, start: usize, quote: char) -> Option<usize> {
    let mut chars = line[start + quote.len_utf8()..].char_indices();
    while let Some((offset, ch)) = chars.next() {
        if ch == '\\' {
            // A backslash escapes the next char (including the quote).
            chars.next();
            continue;
        }
        if ch == quote {
            return Some(start + quote.len_utf8() + offset + quote.len_utf8());
        }
    }
    None
}

fn scan_number(line: &str, start: usize) -> usize {
    let bytes = line.as_bytes();
    let mut end = start;
    while end < bytes.len() {
        let byte = bytes[end];
        if byte.is_ascii_alphanumeric()
            || byte == b'_'
            || byte == b'.'
            || ((byte == b'+' || byte == b'-')
                && end > start
                && matches!(bytes[end - 1], b'e' | b'E'))
        {
            end += 1;
        } else {
            break;
        }
    }
    end
}

/// Whether a `/` at this position starts a regex literal rather than a
/// division operator (JS heuristic: after an opening token or `return`).
fn regex_allowed(last_significant: Option<char>, last_word: Option<&str>) -> bool {
    if matches!(
        last_word,
        Some("return")
            | Some("case")
            | Some("typeof")
            | Some("instanceof")
            | Some("in")
            | Some("of")
            | Some("delete")
            | Some("void")
            | Some("yield")
            | Some("await")
            | Some("throw")
            | Some("new")
    ) {
        return true;
    }
    match last_significant {
        None => true,
        Some(ch) => matches!(
            ch,
            '(' | '['
                | '{'
                | ','
                | ';'
                | ':'
                | '='
                | '!'
                | '&'
                | '|'
                | '?'
                | '+'
                | '-'
                | '*'
                | '%'
                | '^'
                | '~'
                | '<'
                | '>'
        ),
    }
}

/// Scan a JS regex literal body + flags; `None` when the line has no closing
/// unescaped `/`.
fn scan_regex(line: &str, start: usize) -> Option<usize> {
    let mut in_class = false;
    let mut escaped = false;
    for (offset, ch) in line[start + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => {
                let mut end = start + 1 + offset + 1;
                while end < line.len() && line.as_bytes()[end].is_ascii_alphabetic() {
                    end += 1;
                }
                return Some(end);
            }
            _ => {}
        }
    }
    None
}

fn triple_delimiter(quote: char) -> &'static str {
    match quote {
        '"' => "\"\"\"",
        _ => "'''",
    }
}

fn push_span(spans: &mut Vec<Span>, start: usize, end: usize, color: ThemeColor) {
    if start < end {
        spans.push((start, end, color));
    }
}

/// Coalesce directly adjacent runs of the same scope (punctuation runs,
/// keyword-punctuation boundaries, …) so every scope becomes one styled run.
fn coalesce(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for (start, end, color) in spans {
        if start >= end {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.1 == start && last.2 == color {
                last.1 = end;
                continue;
            }
            if last.1 > start {
                // Overlap can only come from a scanner bug; keep the first.
                continue;
            }
        }
        out.push((start, end, color));
    }
    out
}
