//! Unsafe block gate (U2): `#![deny(unsafe_code)]` in `src/lib.rs` stops new
//! `unsafe` outside the reviewed platform/FFI modules, but a module- or
//! function-level `#[allow(unsafe_code)]` (U1) also switches the lint off for
//! everything inside the allowlisted region. This integration test closes that
//! hole at block granularity: it scans every `src/**/*.rs`, counts the real
//! `unsafe {`, `unsafe fn`, `unsafe impl` constructs (comments and string /
//! char / raw literals are ignored), and compares the per-file counts with the
//! checked-in ceiling in `tests/unsafe_gate_allowlist.txt`.
//!
//! Rules:
//! - a file containing unsafe with no allowlist entry fails (new file);
//! - a file whose count exceeds its ceiling fails and prints `file:line` for
//!   every current site in that file;
//! - a file below its ceiling passes (an unsafe block was removed) and the
//!   test prints a notice to tighten the allowlist — code getting safer must
//!   not fail the gate.
//!
//! Updating the allowlist is only for a reviewed platform/FFI boundary with a
//! `// SAFETY:` note at the site. Print the inventory for the current tree
//! with:
//!
//! ```text
//! cargo test --test unsafe_gate -- --ignored --nocapture print_current_unsafe_allowlist
//! ```
//!
//! then replace the data lines in `tests/unsafe_gate_allowlist.txt` and review
//! the diff. Format (one line per file, `#` starts a comment):
//!
//! ```text
//! <path relative to src/, forward slashes> <blocks> <unsafe fns> <unsafe impls/traits>
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const ALLOWLIST_RELATIVE_PATH: &str = "tests/unsafe_gate_allowlist.txt";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn src_dir() -> PathBuf {
    manifest_dir().join("src")
}

fn allowlist_path() -> PathBuf {
    manifest_dir().join(ALLOWLIST_RELATIVE_PATH)
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum UnsafeKind {
    /// `unsafe { ... }` (also `unsafe extern "C" { ... }`).
    Block,
    /// `unsafe fn ...` (also `unsafe extern "C" fn ...`).
    Fn,
    /// `unsafe impl ...` / `unsafe trait ...` (declarations, not blocks).
    Impl,
}

impl UnsafeKind {
    fn label(self) -> &'static str {
        match self {
            UnsafeKind::Block => "unsafe block",
            UnsafeKind::Fn => "unsafe fn",
            UnsafeKind::Impl => "unsafe impl/trait",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UnsafeSite {
    /// 1-based line of the `unsafe` keyword.
    line: u32,
    kind: UnsafeKind,
}

/// Per-file unsafe inventory. Used both for the observed counts and for the
/// allowlist ceilings; comparisons are component-wise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UnsafeCounts {
    blocks: u32,
    fns: u32,
    impls: u32,
}

impl UnsafeCounts {
    fn add(&mut self, kind: UnsafeKind) {
        match kind {
            UnsafeKind::Block => self.blocks += 1,
            UnsafeKind::Fn => self.fns += 1,
            UnsafeKind::Impl => self.impls += 1,
        }
    }

    /// True when any component is above its ceiling. (A shift between kinds,
    /// e.g. one block turned into one unsafe fn, is caught because the fn
    /// ceiling is also compared.)
    fn exceeds(self, ceiling: UnsafeCounts) -> bool {
        self.blocks > ceiling.blocks || self.fns > ceiling.fns || self.impls > ceiling.impls
    }

    fn describe(self) -> String {
        format!(
            "{} unsafe block(s), {} unsafe fn, {} unsafe impl/trait",
            self.blocks, self.fns, self.impls
        )
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Replace comments and string/char/raw literals with spaces, preserving every
/// newline so line numbers in the masked text match the original file.
fn mask_comments_and_strings(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];

        // Line comment: `//` through end of line (also `///`, `//!`).
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            push_blank(&mut out, &chars[start..i]);
            continue;
        }

        // Block comment, which Rust nests. `/* */` including `/**`, `/*!`.
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < chars.len() && depth > 0 {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            push_blank(&mut out, &chars[start..i]);
            continue;
        }

        // Raw string (`r"..."`, `r#"..."#`, `br#"...`#`, `cr#"..."#`).
        if let Some(end) = raw_string_end(&chars, i) {
            let start = i;
            i = end;
            push_blank(&mut out, &chars[start..i]);
            continue;
        }

        // Normal string (`"..."`, `b"..."`, `c"..."` — the b/c prefix is
        // ordinary code, the quote starts the literal).
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() {
                match chars[i] {
                    '\\' => i = (i + 2).min(chars.len()),
                    '"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            push_blank(&mut out, &chars[start..i]);
            continue;
        }

        // Character literal or lifetime. Lifetimes are extremely common and
        // must stay code: `'a` is not `'a'`.
        if c == '\'' {
            if let Some(end) = char_literal_end(&chars, i) {
                let start = i;
                i = end;
                push_blank(&mut out, &chars[start..i]);
                continue;
            }
            out.push('\'');
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

fn push_blank(out: &mut String, slice: &[char]) {
    for ch in slice {
        out.push(if *ch == '\n' { '\n' } else { ' ' });
    }
}

/// If `chars[i]` starts a raw string, return the index one past its closing
/// delimiter. Prefixes: `r`, `br`, `cr`, each followed by zero or more `#`
/// and a `"`. `r#ident` (raw identifier) is not a raw string.
fn raw_string_end(chars: &[char], i: usize) -> Option<usize> {
    let r_index = match chars.get(i) {
        Some('r') => i,
        Some('b') | Some('c') if chars.get(i + 1) == Some(&'r') => i + 1,
        _ => return None,
    };
    // `r`/`b`/`c` must begin a token, not tail an identifier (`var#` etc.).
    if i > 0 && is_ident_continue(chars[i - 1]) {
        return None;
    }

    let mut j = r_index + 1;
    let mut hashes = 0usize;
    while chars.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if chars.get(j) != Some(&'"') {
        return None;
    }
    j += 1;

    while j < chars.len() {
        if chars[j] == '"' {
            let mut k = j + 1;
            let mut closing = 0usize;
            while closing < hashes && chars.get(k) == Some(&'#') {
                closing += 1;
                k += 1;
            }
            if closing == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    // Unterminated raw string: mask the rest of the file, like rustc would
    // report an error. The gate is not a parser; be conservative.
    Some(chars.len())
}

/// If `chars[i]` (a `'`) starts a char literal, return the index one past the
/// closing quote. Returns `None` for lifetimes (`'a`, `'static`, `'_`).
fn char_literal_end(chars: &[char], i: usize) -> Option<usize> {
    let next = *chars.get(i + 1)?;
    if next == '\\' {
        // Escaped literal: consume the escape sequence, then require `'`.
        let mut j = i + 2;
        match *chars.get(j)? {
            'x' => j += 3, // \xNN
            'u' => {
                // \u{...}
                j += 1;
                if chars.get(j) == Some(&'{') {
                    while j < chars.len() && chars[j] != '}' {
                        j += 1;
                    }
                    j = (j + 1).min(chars.len());
                }
            }
            _ => j += 1, // \n, \t, \\, \', \", \0, ...
        }
        return (chars.get(j) == Some(&'\'')).then_some(j + 1);
    }
    // A single (possibly multi-byte) character followed by `'`.
    (chars.get(i + 2) == Some(&'\'')).then_some(i + 3)
}

/// Find every real `unsafe` construct in `source`, ignoring comments and
/// literals. The `unsafe` keyword counts only when the next token is `{`
/// (`unsafe {`), `fn` (`unsafe fn`), `impl`, `trait`, or an `extern` block/fn.
/// `unsafe_code` (the lint name), `#[unsafe(...)]` attributes and
/// `#[expect(unsafe_code)]` are not counted.
fn scan_unsafe_sites(source: &str) -> Vec<UnsafeSite> {
    let masked = mask_comments_and_strings(source);
    let chars: Vec<char> = masked.chars().collect();
    let mut sites = Vec::new();
    let mut line = 1u32;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if !is_ident_start(c) {
            i += 1;
            continue;
        }

        let start = i;
        while i < chars.len() && is_ident_continue(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if word != "unsafe" {
            continue;
        }

        let keyword_line = line;
        let Some(kind) = classify_unsafe_tail(&chars, i) else {
            continue;
        };
        sites.push(UnsafeSite {
            line: keyword_line,
            kind,
        });
    }
    sites
}

/// Look at the tokens after an `unsafe` keyword and classify the construct.
/// `from` is the index just past the keyword in the masked text.
fn classify_unsafe_tail(chars: &[char], from: usize) -> Option<UnsafeKind> {
    let mut j = from;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }

    if chars.get(j) == Some(&'{') {
        return Some(UnsafeKind::Block);
    }
    if !chars.get(j).copied().map(is_ident_start).unwrap_or(false) {
        return None;
    }

    let token_start = j;
    while j < chars.len() && is_ident_continue(chars[j]) {
        j += 1;
    }
    let token: String = chars[token_start..j].iter().collect();
    match token.as_str() {
        "fn" => Some(UnsafeKind::Fn),
        "impl" | "trait" => Some(UnsafeKind::Impl),
        "extern" => {
            // The ABI literal (`extern "C"`) was masked to spaces, so after
            // skipping whitespace the next token is the body/`fn` token.
            let mut k = j;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            if chars.get(k) == Some(&'{') {
                return Some(UnsafeKind::Block);
            }
            if chars.get(k).copied().map(is_ident_start).unwrap_or(false) {
                let token_start = k;
                while k < chars.len() && is_ident_continue(chars[k]) {
                    k += 1;
                }
                let after: String = chars[token_start..k].iter().collect();
                if after == "fn" {
                    return Some(UnsafeKind::Fn);
                }
            }
            None
        }
        // `unsafe auto trait` (unstable): still an unsafe declaration.
        "auto" => {
            let mut k = j;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            let token_start = k;
            while k < chars.len() && is_ident_continue(chars[k]) {
                k += 1;
            }
            let after: String = chars[token_start..k].iter().collect();
            (after == "trait").then_some(UnsafeKind::Impl)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Allowlist and gate policy
// ---------------------------------------------------------------------------

struct Observation {
    counts: UnsafeCounts,
    sites: Vec<UnsafeSite>,
}

#[derive(Default)]
struct GateOutcome {
    /// Hard failures: new unsafe file, or a per-file count above its ceiling.
    failures: Vec<String>,
    /// Passing notice: the tree has less unsafe than the allowlist records.
    tighten_hints: Vec<String>,
}

fn parse_allowlist(text: &str, source_label: &str) -> BTreeMap<String, UnsafeCounts> {
    let mut entries = BTreeMap::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Allow a trailing `#` comment after the counts.
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }

        let mut fields = line.split_whitespace();
        let Some(relative) = fields.next() else {
            continue;
        };
        validate_allowlist_path(relative, source_label, line_number);
        let blocks = parse_count(&mut fields, "blocks", source_label, line_number, line);
        let fns = parse_count(&mut fields, "fns", source_label, line_number, line);
        let impls = parse_count(&mut fields, "impls", source_label, line_number, line);
        if fields.next().is_some() {
            panic!(
                "{source_label}:{line_number}: extra field(s) in `{line}` (expected \
                 `<path relative to src/> <blocks> <unsafe fns> <unsafe impls>`)"
            );
        }
        let counts = UnsafeCounts { blocks, fns, impls };
        if entries.insert(relative.to_string(), counts).is_some() {
            panic!("{source_label}:{line_number}: duplicate allowlist entry for `{relative}`");
        }
    }
    entries
}

fn parse_count(
    fields: &mut std::str::SplitWhitespace<'_>,
    name: &str,
    source_label: &str,
    line_number: usize,
    line: &str,
) -> u32 {
    let Some(raw) = fields.next() else {
        panic!(
            "{source_label}:{line_number}: missing `{name}` count in `{line}` (expected \
             `<path relative to src/> <blocks> <unsafe fns> <unsafe impls>`)"
        );
    };
    raw.parse::<u32>().unwrap_or_else(|_| {
        panic!("{source_label}:{line_number}: `{name}` count `{raw}` is not a non-negative integer")
    })
}

fn validate_allowlist_path(relative: &str, source_label: &str, line_number: usize) {
    let invalid = relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.contains(':')
        || !relative.ends_with(".rs")
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if invalid {
        panic!(
            "{source_label}:{line_number}: allowlist path `{relative}` must be a forward-slash \
             path relative to src/ ending in `.rs` (for example `tui/terminal.rs`)"
        );
    }
}

fn render_sites(relative: &str, sites: &[UnsafeSite]) -> String {
    // Upper bound so a broad regression in a large FFI file stays readable.
    const MAX_SITES: usize = 80;
    let mut lines: Vec<String> = sites
        .iter()
        .take(MAX_SITES)
        .map(|site| format!("    src/{relative}:{}: {}", site.line, site.kind.label()))
        .collect();
    if sites.len() > MAX_SITES {
        lines.push(format!(
            "    ... and {} more site(s) in this file",
            sites.len() - MAX_SITES
        ));
    }
    lines.join("\n")
}

fn evaluate_gate(
    allowlist: &BTreeMap<String, UnsafeCounts>,
    observed: &BTreeMap<String, Observation>,
    all_source_files: &BTreeSet<String>,
) -> GateOutcome {
    let mut outcome = GateOutcome::default();

    // 1. A file with unsafe but no reviewed entry is a new unsafe surface.
    for (relative, observation) in observed {
        if !allowlist.contains_key(relative) {
            outcome.failures.push(format!(
                "NEW FILE WITH UNSAFE: src/{relative} has {} but is not in \
                 {ALLOWLIST_RELATIVE_PATH}\n{}",
                observation.counts.describe(),
                render_sites(relative, &observation.sites)
            ));
        }
    }

    // 2. Any component above its per-file ceiling fails.
    for (relative, ceiling) in allowlist {
        let Some(observation) = observed.get(relative) else {
            if all_source_files.contains(relative) {
                outcome.tighten_hints.push(format!(
                    "src/{relative}: allowlist records {} but the file has no unsafe code left — \
                     remove this entry (or set it to `0 0 0`)",
                    ceiling.describe()
                ));
            } else {
                outcome.tighten_hints.push(format!(
                    "src/{relative}: allowlist entry no longer matches any file under src/ — \
                     remove the stale entry"
                ));
            }
            continue;
        };
        if observation.counts.exceeds(*ceiling) {
            outcome.failures.push(format!(
                "INCREASED UNSAFE: src/{relative} now has {} but the allowlist caps it at {}\n{}",
                observation.counts.describe(),
                ceiling.describe(),
                render_sites(relative, &observation.sites)
            ));
        } else if observation.counts != *ceiling {
            outcome.tighten_hints.push(format!(
                "src/{relative}: allowlist records {} but the tree now has {} — tighten the \
                 allowlist",
                ceiling.describe(),
                observation.counts.describe()
            ));
        }
    }

    outcome
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read directory {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "cannot read a directory entry under {}: {error}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

fn relative_source_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or_else(|_| panic!("{} is not under {}", path.display(), root.display()))
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_source(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read source file {}: {error}", path.display()))
}

fn collect_observations() -> (BTreeSet<String>, BTreeMap<String, Observation>) {
    let root = src_dir();
    assert!(
        root.is_dir(),
        "missing src/ directory at {} — the gate walked the wrong root",
        root.display()
    );

    let mut paths = Vec::new();
    collect_rs_files(&root, &mut paths);
    assert!(
        !paths.is_empty(),
        "no .rs files found under {} — the gate walked the wrong root",
        root.display()
    );

    let mut all_files = BTreeSet::new();
    let mut observed = BTreeMap::new();
    for path in paths {
        let relative = relative_source_path(&root, &path);
        let sites = scan_unsafe_sites(&read_source(&path));
        all_files.insert(relative.clone());
        if sites.is_empty() {
            continue;
        }
        let mut counts = UnsafeCounts::default();
        for site in &sites {
            counts.add(site.kind);
        }
        observed.insert(relative, Observation { counts, sites });
    }
    (all_files, observed)
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn no_new_unsafe_blocks_outside_the_reviewed_allowlist() {
    let allowlist_file = allowlist_path();
    let allowlist_text = std::fs::read_to_string(&allowlist_file).unwrap_or_else(|error| {
        panic!(
            "unsafe gate: cannot read the checked-in allowlist {}: {error}\n\
             The gate needs a reviewed inventory of the unsafe constructs that are already in \
             src/. Generate it with:\n  \
             cargo test --test unsafe_gate -- --ignored --nocapture print_current_unsafe_allowlist",
            allowlist_file.display()
        )
    });
    let allowlist = parse_allowlist(&allowlist_text, ALLOWLIST_RELATIVE_PATH);
    let (all_files, observed) = collect_observations();
    let outcome = evaluate_gate(&allowlist, &observed, &all_files);

    if !outcome.failures.is_empty() {
        panic!(
            "unsafe gate failed: src/ contains unsafe code that is not covered by the reviewed \
             allowlist ({ALLOWLIST_RELATIVE_PATH}).\n\
             \n{}\n\
             \nHow to fix:\n  \
             1. Preferred: remove the unsafe. The crate root has `#![deny(unsafe_code)]`; a \
             module- or function-level `#[allow(unsafe_code)]` is only for reviewed FFI/platform \
             boundaries and must not hide a new block.\n  \
             2. If (and only if) the new site is a reviewed platform/FFI boundary, add a \
             `// SAFETY:` comment next to it and raise this file's ceiling in \
             {ALLOWLIST_RELATIVE_PATH} (counts are per file: blocks, unsafe fns, unsafe \
             impls/traits).\n     \
             Print the inventory for the current tree with:\n       \
             cargo test --test unsafe_gate -- --ignored --nocapture \
             print_current_unsafe_allowlist\n  \
             3. Do not widen a module/file `#[allow(unsafe_code)]` to cover new blocks: this gate \
             counts blocks, so an increase always fails regardless of the lint allow.",
            outcome.failures.join("\n\n")
        );
    }

    if !outcome.tighten_hints.is_empty() {
        let body = format!(
            "unsafe gate notice (not a failure): {} allowlist entr{} can be tightened because \
             src/ currently has less unsafe code than recorded:\n{}\n\
             Update {ALLOWLIST_RELATIVE_PATH} to match the current tree.",
            outcome.tighten_hints.len(),
            if outcome.tighten_hints.len() == 1 {
                "y"
            } else {
                "ies"
            },
            outcome.tighten_hints.join("\n")
        );
        // The test passes, so libtest captures `println!` output. Write the
        // notice to the real stderr as well so a removed unsafe is visible in
        // a plain `cargo test` run (it still does not fail the gate).
        println!("{body}");
        let _ = writeln!(std::io::stderr(), "{body}");
    }
}

/// Helper, not part of the pass/fail gate: prints the allowlist body for the
/// current tree. Copy it over the data lines in
/// `tests/unsafe_gate_allowlist.txt`, then review the diff.
#[test]
#[ignore = "helper: prints the current unsafe inventory for tests/unsafe_gate_allowlist.txt"]
fn print_current_unsafe_allowlist() {
    let (_, observed) = collect_observations();
    let mut blocks = 0u32;
    let mut fns = 0u32;
    let mut impls = 0u32;

    println!(
        "# Unsafe-code inventory for tests/unsafe_gate.rs (U2 gate).\n\
         #\n\
         # Each line caps one file under src/ at the reviewed number of constructs.\n\
         # Adding a new `unsafe {{`, `unsafe fn` or `unsafe impl`/`trait` to a listed file\n\
         # fails `no_new_unsafe_blocks_outside_the_reviewed_allowlist`; removing one\n\
         # passes and asks for this file to be tightened.\n\
         #\n\
         # Format: <path relative to src/> <blocks> <unsafe fns> <unsafe impls/traits>\n\
         # Regenerate with:\n\
         #   cargo test --test unsafe_gate -- --ignored --nocapture print_current_unsafe_allowlist"
    );
    for (relative, observation) in &observed {
        let counts = observation.counts;
        blocks += counts.blocks;
        fns += counts.fns;
        impls += counts.impls;
        println!(
            "{relative} {} {} {}",
            counts.blocks, counts.fns, counts.impls
        );
    }
    println!();
    println!(
        "# total: {} file(s), {} unsafe block(s), {} unsafe fn, {} unsafe impl/trait",
        observed.len(),
        blocks,
        fns,
        impls
    );
}

// ---------------------------------------------------------------------------
// Self-tests for the scanner and the gate policy
// ---------------------------------------------------------------------------

#[test]
fn scanner_ignores_comments_and_literals() {
    let source = r###"
// unsafe { in a line comment
// unsafe fn in a line comment
/* unsafe { in a block comment */
/* outer /* nested unsafe { */ still a comment unsafe impl */
/// doc comment: unsafe { }
const A: &str = "unsafe { not code }";
const B: &str = "escaped \" unsafe fn";
const C: char = 'u';
const D: &str = r"raw unsafe { string }";
const E: &str = r#"raw " unsafe impl here"#;
const F: &[u8] = b"unsafe { bytes }";
const G: &[u8] = br#"unsafe fn raw bytes"#;
const H: &str = c"unsafe { c string }";
const I: char = '\'';
const J: char = '\u{1F600}';
#[allow(unsafe_code)]
fn allowed_region() {
    let real = unsafe { 1 };
}
"###;
    let sites = scan_unsafe_sites(source);
    assert_eq!(
        sites,
        vec![UnsafeSite {
            line: 19,
            kind: UnsafeKind::Block
        }],
        "only the real unsafe block on line 19 should be counted"
    );
}

#[test]
fn scanner_classifies_unsafe_constructs() {
    let source = "\
unsafe { one(); }
unsafe{
let _ = unsafe /* comment */ { two() };
unsafe fn helper() {}
pub unsafe fn exported() {}
unsafe impl Send for X {}
unsafe trait Marker {}
unsafe extern \"C\" { fn c_call(); }
unsafe extern \"C\" fn c_fn() {}
#[expect(unsafe_code, reason = \"x\")]
let name = \"unsafe_code\";
";
    let sites = scan_unsafe_sites(source);
    let kinds: Vec<UnsafeKind> = sites.iter().map(|site| site.kind).collect();
    assert_eq!(
        kinds,
        vec![
            UnsafeKind::Block,
            UnsafeKind::Block,
            UnsafeKind::Block,
            UnsafeKind::Fn,
            UnsafeKind::Fn,
            UnsafeKind::Impl,
            UnsafeKind::Impl,
            UnsafeKind::Block,
            UnsafeKind::Fn,
        ]
    );
    let lines: Vec<u32> = sites.iter().map(|site| site.line).collect();
    assert_eq!(lines, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn scanner_does_not_flag_lint_names_or_attributes() {
    let source = "\
#[allow(unsafe_code)]
#[expect(unsafe_code)]
#[unsafe(no_mangle)]
fn f<'a, 'static>(x: &'a str) -> usize { x.len() }
let has_unsafe_in_its_name = 0;
";
    assert!(scan_unsafe_sites(source).is_empty());
}

#[test]
fn simulated_gate_rejects_new_or_growing_unsafe() {
    let ceiling = UnsafeCounts {
        blocks: 2,
        fns: 1,
        impls: 0,
    };
    let mut allowlist = BTreeMap::new();
    allowlist.insert("ffi/a.rs".to_string(), ceiling);
    let all_files: BTreeSet<String> = ["ffi/a.rs", "core/b.rs"]
        .into_iter()
        .map(str::to_string)
        .collect();

    let observed_at_ceiling = |counts: UnsafeCounts| Observation {
        counts,
        sites: vec![
            UnsafeSite {
                line: 10,
                kind: UnsafeKind::Block,
            },
            UnsafeSite {
                line: 20,
                kind: UnsafeKind::Block,
            },
            UnsafeSite {
                line: 30,
                kind: UnsafeKind::Fn,
            },
        ],
    };

    // At the ceiling: passes.
    let mut observed = BTreeMap::new();
    observed.insert("ffi/a.rs".to_string(), observed_at_ceiling(ceiling));
    let outcome = evaluate_gate(&allowlist, &observed, &all_files);
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert!(
        outcome.tighten_hints.is_empty(),
        "{:?}",
        outcome.tighten_hints
    );

    // One extra block: fails and prints file/line.
    let mut observed = BTreeMap::new();
    observed.insert(
        "ffi/a.rs".to_string(),
        observed_at_ceiling(UnsafeCounts {
            blocks: 3,
            fns: 1,
            impls: 0,
        }),
    );
    let outcome = evaluate_gate(&allowlist, &observed, &all_files);
    assert_eq!(outcome.failures.len(), 1);
    let failure = &outcome.failures[0];
    assert!(failure.contains("INCREASED UNSAFE"), "{failure}");
    assert!(failure.contains("src/ffi/a.rs"), "{failure}");
    assert!(
        failure.contains("src/ffi/a.rs:10: unsafe block"),
        "{failure}"
    );
    assert!(failure.contains("src/ffi/a.rs:30: unsafe fn"), "{failure}");

    // A new file with unsafe: fails even when no entry exists.
    let mut observed = BTreeMap::new();
    observed.insert(
        "core/b.rs".to_string(),
        observed_at_ceiling(UnsafeCounts {
            blocks: 1,
            fns: 0,
            impls: 0,
        }),
    );
    let outcome = evaluate_gate(&allowlist, &observed, &all_files);
    assert_eq!(outcome.failures.len(), 1);
    assert!(
        outcome.failures[0].contains("NEW FILE WITH UNSAFE"),
        "{}",
        outcome.failures[0]
    );

    // One block removed: passes with a tightening notice.
    let mut observed = BTreeMap::new();
    observed.insert(
        "ffi/a.rs".to_string(),
        observed_at_ceiling(UnsafeCounts {
            blocks: 1,
            fns: 1,
            impls: 0,
        }),
    );
    let outcome = evaluate_gate(&allowlist, &observed, &all_files);
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(outcome.tighten_hints.len(), 1);
    assert!(outcome.tighten_hints[0].contains("tighten the allowlist"));

    // An allowlisted file missing from the tree: passes with a stale-entry
    // notice, never a failure.
    let outcome = evaluate_gate(&allowlist, &BTreeMap::new(), &all_files);
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(outcome.tighten_hints.len(), 1);
}

#[test]
fn allowlist_parser_reads_counts_and_rejects_malformed_lines() {
    assert!(parse_allowlist("", "test").is_empty());
    assert!(parse_allowlist("# comment only\n\n", "test").is_empty());
    let parsed = parse_allowlist(
        "# header\nffi/a.rs 2 1 0\ncore/b.rs 0 0 0 # trailing comment is allowed\n",
        "test",
    );
    assert_eq!(
        parsed.get("ffi/a.rs"),
        Some(&UnsafeCounts {
            blocks: 2,
            fns: 1,
            impls: 0
        })
    );

    for (text, needle) in [
        ("ffi/a.rs 2 1\n", "missing `impls` count"),
        ("ffi/a.rs 2 1 0 extra\n", "extra field"),
        (
            "ffi/a.rs 2 1 0\nffi/a.rs 0 0 0\n",
            "duplicate allowlist entry",
        ),
        ("/abs.rs 0 0 0\n", "must be a forward-slash path"),
        ("core/b.rs 0 0 zero\n", "not a non-negative integer"),
    ] {
        let result = std::panic::catch_unwind(|| parse_allowlist(text, "test"));
        let error = result.expect_err(&format!("expected panic for {text:?}"));
        let message = panic_message(&error);
        assert!(message.contains(needle), "{message:?} lacks {needle:?}");
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}
