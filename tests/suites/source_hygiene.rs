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

/// Inline tests are **never** allowed: every test lives under `tests/`, so the
/// allowlist is a migration-debt ledger, not a permanent exemption. (The TUI
/// files that used to be grandfathered here now live in
/// `tests/suites/tui_units.rs`.)
///
/// 2026-11-11 release-prep sweep: the 20 files reported by this gate were
/// re-audited one by one. Ten were moved out unchanged (their helpers are
/// `pub`): `tui/autocomplete.rs`, `tui/app.rs`, `tui/core.rs`,
/// `tui/settings_menu.rs`, `tui/spill_preview.rs`, `tui/alloc_trace.rs` →
/// `tests/suites/tui_surface_units.rs` + `tests/suites/tui_alloc_trace_units.rs`;
/// `config/settings/env.rs` + `config/settings/schema.rs` →
/// `tests/suites/config_settings_units.rs`; `coding_agent/tool_selection.rs` →
/// `tests/suites/coding_agent_units.rs`; `plugins/builtin/phone/session.rs` →
/// `tests/suites/phone_units.rs`; plus `tui/terminal.rs`'s
/// `no_model_message_tests` → `tui_surface_units.rs`.
///
/// 2026-11-11 closer sweep: the other ten test a private item, so each got
/// the repo's sanctioned `#[doc(hidden)] pub` seam and moved too —
/// `agent/agent_loop.rs` → `agent_loop_units.rs`, `agent_session/session_ctx.rs`
/// → `session_ctx_units.rs`, `coding_agent/context.rs` + `tools/bash.rs` +
/// `tools/util.rs` → `coding_agent_units.rs`, `plugins/builtin/phone/{mod,remote}.rs`
/// → `phone_units.rs`, `sdk/event.rs` → `sdk_event_units.rs`,
/// `tui/messages.rs` → `tui_messages_units.rs`, `tui/terminal.rs`'s
/// `fork_seed_tests` → `tui_surface_units.rs`. What remains is the original
/// 2026-11-01 grandfathered set only; every entry still has inline tests and
/// is tracked as migration debt, not a permanent exemption.
const PREEXISTING_INLINE_TEST_ALLOWLIST: [&str; 8] = [
    "ai/api/bedrock/credentials.rs",
    "ai/api/partial_mirror.rs",
    "durable/storage/jsonl.rs",
    "env_client/selection.rs",
    "extensions/loader.rs",
    "main_cli/cli_preprocess.rs",
    "plugins/builtin/mcp.rs",
    "session_manager/search.rs",
];

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
const EXPECT_SITE_ALLOWLIST: [(&str, u32); 30] = [
    ("plugins/tool_repair/grammar.rs", 196),
    ("ai/api/constrained_sampling.rs", 178),
    // These three shifted down when the queue-bounds work added the
    // park/release paths above them; they are the same `OpOk`/`OpErr`/
    // `PollEnvelope` invariants documented above, re-justified by line.
    // Shifted 329/339/679 → 359/369/709 by the `WORKER_JS_STACK_BYTES`
    // constant + compile-time assertion block (same three invariants), then
    // 359/369/709 → 368/378/718 by the C2 deflated worker-boot seam, then
    // 368/378/718 → 452/462/819 by the generation-release and worker-limit
    // additions above (same `OpOk`/`OpErr`/`PollEnvelope` invariants), then
    // 452/462/819 → 753/763/1187 by the cross-generation LIVE_WORKERS
    // guard/accounting work (same three invariants).
    ("extensions/runtime/host_functions/worker_fns.rs", 753),
    ("extensions/runtime/host_functions/worker_fns.rs", 763),
    ("extensions/runtime/host_functions/worker_fns.rs", 1187),
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
    ("durable/harness/generation.rs", 317),
    // The task definition build, the `pi.user` draft and the
    // `pi.assistant` draft: same invariants as before, shifted by the
    // handoff/continuation and streamed-projection edits above them, then by
    // the incremental-partial clone fix (2525 → 2736 → 2740,
    // 2752 → 2965 → 2969).
    ("durable/harness/generation.rs", 611),
    ("durable/harness/generation.rs", 2740),
    ("durable/harness/generation.rs", 2969),
    ("durable/harness/inbox.rs", 289),
    ("durable/harness/live.rs", 320),
    ("durable/harness/prompt.rs", 197),
    ("durable/harness/provider.rs", 70),
    ("durable/harness/registry.rs", 524),
    ("durable/harness/registry.rs", 564),
    ("durable/harness/tool.rs", 521),
    ("durable/harness/tool.rs", 529),
    ("durable/harness/usage.rs", 155),
    ("durable/harness/view.rs", 223),
    // The session-tree flat arena is addressed by `u32` node indices; a real
    // session cannot exceed `u32::MAX` nodes (each node is one JSONL entry),
    // so the conversion failing means the fixture itself is invalid.
    // Shifted 723 -> 746 by the R57 CachedTree head/tail edits above it in the
    // same function (same site, same invariant), then 746 -> 780 by the
    // q4-memory-ceiling JS-face snapshot gate (the `js_face` input field and
    // `serialize_projection_snapshot` doc/signature above it).
    ("agent_session/session_ctx.rs", 780),
    // `reuse` is computed two lines above as
    // `old.as_ref().is_some_and(|slot| ..)`, so `Some` is proven when this
    // branch runs; the `take` cannot observe `None`. Shifted 8398 -> 8458 by
    // the PIR-TUI-027/030 edits above it in the same function, then
    // 8458 -> 8509 by the PIR-TUI-034/035/036/039 window and dock edits, then
    // 8509 -> 8507 by the abort-retry/streaming edits (same site, same
    // invariant).
    ("tui/terminal.rs", 8507),
    // `guard.sender.is_none()` returns above, so the `as_ref()` in the same
    // locked scope is `Some`: the sender is read and used under one sink lock
    // with no intervening mutation. (This site surfaced when the file's
    // inline test module moved to `tests/`: the gate's inline-module
    // exemption ran from the first `#[cfg(test)]` line, which used to sit
    // above it.) Shifted 1198 -> 1200 by `cargo fmt` reflowing the same
    // `as_ref()` call (same site, same invariant).
    ("sdk/event.rs", 1200),
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
        // Inline test modules are tracked migration debt above; their
        // `.expect(` sites are test assertions, not production contracts.
        // Scope the exemption to the module itself: a new `.expect(` in the
        // production half of an allowlisted file must still fail the gate.
        // All current inline modules are the last item in their file, so the
        // first `#[cfg(test)]` line marks the start of the exempt region.
        let inline_test_start =
            if PREEXISTING_INLINE_TEST_ALLOWLIST.contains(&relative_text.as_str()) {
                text.lines()
                    .position(|line| line.trim_start().starts_with("#[cfg(test)]"))
                    .map(|index| index + 1)
            } else {
                None
            };
        for (index, line) in text.lines().enumerate() {
            if inline_test_start.is_some_and(|start| index + 1 >= start) {
                continue;
            }
            // Documentation examples are prose, not executable production
            // code; skip line comments and doc comments.
            if line.trim_start().starts_with("//") {
                continue;
            }
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
