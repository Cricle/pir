//! Source-tree hygiene gate: all tests must live in `tests/`, never inline in
//! `src/` (`#[cfg(test)]` modules are a migration debt this gate forbids).
//!
//! Walks every file under `src/` at runtime (relative to
//! `CARGO_MANIFEST_DIR`) and fails when a file contains `#[test]`,
//! `#[tokio::test]`, or `#[cfg(test)]`. It also keeps `src/` to Rust sources
//! plus the embedded catalog resources. (Forbidden upstream artifacts —
//! `.ts`/`.js`/package manifests — are covered repo-wide by
//! `forbidden_files.rs`.)

use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect every file under `dir`.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("src/ must be readable");
    for entry in entries {
        let entry = entry.expect("directory entry readable");
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

const FORBIDDEN_MARKERS: [&str; 3] = ["#[test]", "#[tokio::test]", "#[cfg(test)]"];

/// Inline tests are **never** allowed: every test lives under `tests/`, so this
/// allowlist is permanently empty. (It used to grandfather two TUI files while
/// their suites were being migrated; those suites now live in
/// `tests/suites/tui_units.rs`.)
const PREEXISTING_INLINE_TEST_ALLOWLIST: [&str; 0] = [];

#[test]
fn src_tree_contains_no_inline_test_modules() {
    let root = src_dir();
    assert!(
        root.is_dir(),
        "missing src/ directory at {}",
        root.display()
    );

    let mut files = Vec::new();
    collect_files(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no files found under src/ — the gate walked the wrong root"
    );

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let relative = path.strip_prefix(&root).unwrap();
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read src file {}: {error}", path.display()));
        for marker in FORBIDDEN_MARKERS {
            if text.contains(marker)
                && !PREEXISTING_INLINE_TEST_ALLOWLIST.contains(&relative_text.as_str())
            {
                violations.push(format!("{} contains {marker}", relative.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "inline tests are forbidden in src/ (all tests belong in tests/):\n{}",
        violations.join("\n")
    );
}

#[test]
fn src_tree_contains_only_expected_file_kinds() {
    let root = src_dir();
    let mut files = Vec::new();
    collect_files(&root, &mut files);

    // Rust sources plus the embedded provider-catalog resources
    // (`src/ai/providers/catalog/*.json`, included via `include_str!`) and
    // the extension-host shim sources (`src/extensions/api/js/*.js.txt`,
    // also `include_str!`-embedded — the 2026-10-12 mandated .js.txt
    // architecture keeps raw JS out of the hygiene gate for .js/.ts).
    // Other upstream artifacts (.ts/.js/package manifests) are covered
    // repo-wide by `forbidden_files.rs`.
    let allowed_json_dir = root.join("ai").join("providers").join("catalog");
    let allowed_js_txt_dir = root.join("extensions").join("api").join("js");
    // The native pi-phone port embeds the reference browser UI verbatim
    // (`include_bytes!`), so its .js/.html/.css/.svg/.webmanifest/.md assets
    // live next to the Rust source by design. Scope the carve-out to exactly
    // this directory; nothing else under src/ may carry non-Rust files.
    let allowed_phone_assets_dir = root
        .join("plugins")
        .join("builtin")
        .join("phone")
        .join("assets");
    let unexpected: Vec<String> = files
        .iter()
        .filter(|path| {
            let is_rs = path.extension().is_some_and(|ext| ext == "rs");
            let is_catalog_json = path.extension().is_some_and(|ext| ext == "json")
                && path
                    .parent()
                    .is_some_and(|parent| parent == allowed_json_dir);
            let is_shim_txt = path.extension().is_some_and(|ext| ext == "txt")
                && path
                    .parent()
                    .is_some_and(|parent| parent == allowed_js_txt_dir);
            let is_phone_asset = path.starts_with(&allowed_phone_assets_dir);
            !(is_rs || is_catalog_json || is_shim_txt || is_phone_asset)
        })
        .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    assert!(
        unexpected.is_empty(),
        "src/ contains unexpected non-source files: {unexpected:?}"
    );
}

/// Explicit allowlist of `.expect(` sites permitted in production code, as
/// `("repo-relative path", line)`. Each entry is an invariant the surrounding
/// code proves at runtime (a value checked a few lines above); anything else
/// must use infallible constructs or explicit error handling. Stale entries
/// (site removed or shifted by an owner fix) are tolerated; NEW sites fail.
///
/// - `plugins/tool_repair/grammar.rs` — `Regex::new` on a `&'static str`
///   pattern in the private `regex()` helper: a literal cannot fail to
///   compile, and a typo is caught by the suite on first use instead of
///   silently dropping a repair family. (The former
///   `agent/tool_scheduler/prepare.rs` entries are gone: those sites were
///   rewritten into `if let Some(..) = value.as_object()`/`as_array()`
///   bindings, so no `.expect(` remains in that file.)
/// - `ai/api/constrained_sampling.rs` — checked-above schema-shape invariant.
/// - `extensions/runtime/host_functions/worker_fns.rs` — `OpOk`/`OpErr`/
///   `PollEnvelope` envelopes: `bool`/`&str`/`Value`/`Option<i32>` fields,
///   struct-serialized with literal field names into an in-memory writer, so
///   every `serde_json` failure source (writer I/O, map keys, fallible custom
///   `Serialize`, `arbitrary_precision`) is absent. `expect` replaces silent
///   fallbacks that could discard an op's value or message.
/// - `extensions/runtime/host_functions/context_fns.rs` — `CompleteOk
///   <AssistantMessage>`: a derive-only data tree (no fallible custom
///   `Serialize`) serialized into an in-memory writer.
// Durable-harness definition construction: `build` validates a fixed
// definition (constants compiled into the crate), so failure is a
// programming error surfaced immediately rather than a runtime condition.
// The serde sites serialize the crate's own derive-only message/checkpoint
// types into memory.
const EXPECT_SITE_ALLOWLIST: [(&str, u32); 27] = [
    ("plugins/tool_repair/grammar.rs", 196),
    ("ai/api/constrained_sampling.rs", 178),
    // These three shifted down when the queue-bounds work added the
    // park/release paths above them; they are the same `OpOk`/`OpErr`/
    // `PollEnvelope` invariants documented above, re-justified by line.
    // Shifted 329/339/679 → 359/369/709 by the `WORKER_JS_STACK_BYTES`
    // constant + compile-time assertion block (same three invariants), then
    // 359/369/709 → 368/378/718 by the C2 deflated worker-boot seam, then
    // 368/378/718 → 452/462/819 by the generation-release and worker-limit
    // additions above (same `OpOk`/`OpErr`/`PollEnvelope` invariants).
    ("extensions/runtime/host_functions/worker_fns.rs", 452),
    ("extensions/runtime/host_functions/worker_fns.rs", 462),
    ("extensions/runtime/host_functions/worker_fns.rs", 819),
    // Shifted 586 → 593 when the deep-session work changed getSessionManagerJson
    // to splice the pre-rendered tree, and 593 → 655 when the scalar
    // `ctx.sessionManager` host functions were added above it (same
    // CompleteOk<AssistantMessage> invariant).
    ("extensions/runtime/host_functions/context_fns.rs", 655),
    // Static regex patterns (`&'static str` literals compiled once in a
    // `OnceLock`): a literal cannot fail to compile, and a typo is caught by
    // the context-mode / phone tests on first use rather than silently
    // dropping a transform. Same invariant as the grammar.rs entry above.
    ("agent_session/phone_child.rs", 43),
    ("plugins/builtin/context_mode/fetch.rs", 200),
    ("plugins/builtin/context_mode/fetch.rs", 208),
    ("plugins/builtin/context_mode/fetch.rs", 213),
    ("plugins/builtin/context_mode/fetch.rs", 228),
    ("durable/harness/documents.rs", 128),
    ("durable/harness/documents.rs", 145),
    ("durable/harness/generation.rs", 315),
    ("durable/harness/generation.rs", 598),
    ("durable/harness/generation.rs", 2525),
    ("durable/harness/generation.rs", 2752),
    ("durable/harness/harness.rs", 1795),
    ("durable/harness/inbox.rs", 289),
    ("durable/harness/live.rs", 320),
    ("durable/harness/prompt.rs", 197),
    ("durable/harness/provider.rs", 70),
    ("durable/harness/registry.rs", 524),
    ("durable/harness/registry.rs", 564),
    ("durable/harness/tool.rs", 520),
    ("durable/harness/tool.rs", 528),
    ("durable/harness/usage.rs", 155),
];

/// Hygiene gate: production code may contain `.expect(` only at the explicit
/// file:line allowlist above. Line-scoped so a moved site fails loudly and
/// must be re-justified by the owning stream.
#[test]
fn src_tree_expect_sites_match_the_allowlist() {
    let root = src_dir();
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no files found under src/ — the gate walked the wrong root"
    );

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let relative = path.strip_prefix(&root).unwrap();
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read src file {}: {error}", path.display()));
        for (index, line) in text.lines().enumerate() {
            // The turbofish (`.expect::<T>(`) and spaced (`.expect (`)
            // spellings are the same construct to a reader — match them too so
            // the gate cannot be evaded by formatting.
            let expect_site = line.contains(".expect(")
                || line.contains(".expect::<")
                || line.contains(".expect (");
            if expect_site {
                let line_number = (index + 1) as u32;
                let allowed = EXPECT_SITE_ALLOWLIST
                    .iter()
                    .any(|(file, line)| *file == relative_text.as_str() && *line == line_number);
                if !allowed {
                    violations.push(format!("{}:{}: {line}", path.display(), index + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "unexpected `.expect(` in production code (only the explicit allowlist \
         in tests/source_hygiene.rs is permitted; use infallible constructs or \
         explicit handling instead):\n{}",
        violations.join("\n")
    );
}
