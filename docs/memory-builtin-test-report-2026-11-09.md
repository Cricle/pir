# `builtin:memory` post-integration test report — 2026-11-09

Scope: `pir` linked against the published **cm-memory 0.3.1** registry crate
(`sha256 ac9b89e020958530b9faf8a5deb0c1bc3c3e93549beee7b12c683a8819cc536f`),
not the local path checkout. Provider for live A/B: `e6t/deepseek-v4.1-flash`.

## Build matrix

| Check | Result |
|---|---|
| `cargo check --features full` | pass |
| `cargo check --no-default-features --features memory` | pass |
| `cargo build` (full, registry cm-memory) | pass |
| `cargo tree -p cm-memory` | `cm-memory v0.3.1` from registry, no path |
| `cargo package` (cm-rs 0.3.1) | pass |
| GitHub tag/release `v0.3.1` | published; crates.io 0.3.1 visible |

## Test matrix

| Suite | Passed | Failed | Notes |
|---|---:|---:|---|
| `tools/cm-rs cargo test` (all) | 123 | 0 | library + CLI + store + absorb + wiki |
| `tools/cm-rs --test wiki` | 32 | 0 | plan/generate/review/apply/stale checks |
| `tools/cm-rs --test ops_api` | 6 | 0 | full library ops, import hardening, replace-all, concurrency |
| `pir memory_builtin` | 14 | 0 | tool/command, auto-recall policies, auto-brief, session path, read-only, trust/meta hardening, wiki action |
| `pir builtin_cli_audit` | 8 | 0 | builtin surface/default plan/feature diagnostics |
| `pir plugins_units` | 23 | 0 | registry/event/tool semantics |
| `pir plugins_unified` | 21 | 0 | Rust+JS unified registry |
| `pir sdk` | 97 | 0 | SDK builtin wiring/isolation |
| `pir plugin_events_w16` | 17 | 2 | phone-only drift: concurrent phone edits changed `phone` to `EventInterest::All` and added `terminal_input`; memory's audited event set is green |
| `pir zz_merged_6` (full merged target) | 994 | 2 | same two phone-only event-audit failures; memory tests all green |

`pir` full `cargo test --features memory --no-fail-fast` was also run across
15 test targets: **7038 passed, 72 failed, 43 ignored**. The failures are in
pre-existing/unrelated TUI, phone-asset, extension and print-mock areas (the
workspace has concurrent phone asset deletions); no memory suite failed. The
memory-specific rows above are the authoritative signal for this integration.
A representative unrelated failure is `durable_storage::jsonl_recovers...`,
which passes when run alone; two `plugin_events_w16` failures are the phone
`EventInterest::All`/`terminal_input` drift caused by concurrent phone edits.

## Wiki generation metrics

Strict deterministic demo repository (one README target):

| Step | Metric |
|---|---:|
| `wiki plan` | 1 page, 1 target |
| `wiki generate` | attempted 1, written 1, failed 0 |
| `wiki check --strict` | checked 1, issues 0, exit 0 |
| `wiki status` | written 1, planned 0, stale 0, review_failed 0 |
| `wiki find` | 1 page, top score 6.57 |
| `wiki excerpt` | page overview, matching sections extracted |

Real `pir` repository plan (`--max-modules 12`):

| Metric | Value |
|---|---:|
| Planned pages | 17 |
| Target references | 178 |
| Unique target files | 168 |
| Page kinds | overview 1, architecture 1, operations 1, testing 1, flows 1, module 12 |
| Strict check before generation | exit 1: "wiki is incomplete: 17 page(s) still planned" |
| Synthetic apply stress test | 17/17 pages written; strict content-quality issues remain because the pages are synthetic, not model-authored |

A missing-file filter was added to the local wiki planner (not part of the
already-published cm-memory 0.3.1): a fresh plan now drops git-tracked paths
that no longer exist. On the same `pir` tree the plan changed from **8 pages /
86 target references with 14 missing paths** to **8 pages / 72 target
references with 0 missing paths** (phone module 19→6 targets, architecture
8→7).

Real model generation on a smaller clean `pir` plan (`--max-modules 3`, 8 pages / 72 target references / 0 missing):

| Metric | Value |
|---|---:|
| Generated page files | **8/8**: overview 14,434 B; architecture 15,412 B; build-and-test 12,153 B; testing 13,764 B; key-flows 16,509 B; module-scenarios 24,605 B; module-phone 16,533 B; module-play 14,495 B |
| Final status | **written 8, planned 0, stale 0** |
| `wiki check --strict` | **exit 0, 0 issues** |
| `path:line` citations | **1,021**, all resolve after unique-basename resolution; 0 unresolved/out-of-range |
| Module-scenarios repair | model output had 62 bare `:line` refs and 4 invalid paths; rewritten to full repository-relative citations, then applied |
| Generation accounting | 7 pages model-generated; module-scenarios model-generated then citation-repaired |

The clean deterministic strict demo above proves the plan→generate→apply→check
pipeline. The real `pir` run shows the acceptance behavior: bad citations are
rejected by the mechanical gate, and the final repaired 8-page wiki passes strict
check. This is a **mechanical** pass; semantic spot-checks of the same plan
earlier found several imprecise line citations (13/23 direct, 4/23 imprecise,
5/23 wrong line, 1/23 false surrounding claim), so it is not a proof of perfect
prose accuracy.

Why the final green state is only mechanical: `wiki check` explicitly checks
page shape and citation existence, not whether the cited line supports the
sentence (`src/wiki.rs` check doc comment). `wiki apply` recomputes
`source_hash` from the current targets and sets `status="written"` without
regenerating prose (`src/wiki.rs` apply). Therefore manually re-applying pages
after source edits clears `stale` and makes strict check green while leaving
the prose older than the current source. A real semantic pass needs
`wiki review`/an independent reviewer, not `wiki check` alone.

The LLM reviewer confirms this directly: `wiki review` on the generated
`overview` page returned **verdict=fail, score=32**, with wrong-line citations
such as `Cargo.toml:378` vs actual `:404`, `src/main.rs:64` vs `:50`, and
several SDK/agent-loop lines off by 2–30 lines.

On the **stable upstream `pi` source** (`C:\wp\empty\pi`, no concurrent edits),
the reviewed overview run is also rejected by a different set of semantic
issues, so staleness is not the only cause:

- claims a cross-workspace `version` is not tied to package versions, while
  `package.json` has `version:patch/minor/major/set` scripts
- claims the TUI package is consumed by the CLI without citing a dependency
  declaration
- says the README package table lists seven “published” packages; the table does
  not establish publication and `packages/evals` is private

`written=0` with reviewer verdict fail is the correct behavior: the semantic
gate is refusing a polished but overgeneralized page.

Two additional generator bugs were found while reproducing this on stable pi:

- `pir print` expands `@file` references in its prompt, so source excerpts
  containing npm scopes (`@earendil-works/...`) failed or stalled. Source
  excerpts now render scoped names with a fullwidth at-sign (the model sees the
  text; the print host does not treat it as an attachment).
- The eval model's `maxTokens` was 8192; long wiki pages were truncated. A
  wiki-specific agent dir with `maxTokens: 32768` produced complete pages.
  The generator prompt was tightened and the mechanical evidence map enlarged
  (80→240 declaration lines, 12→20 symbols per target). `wiki check` is
  mechanical only; reviewed generation is still required.

Final stable-pi wiki state (`C:\wp\empty\.wiki-eval\pi-upstream-wiki`):

| Metric | Value |
|---|---:|
| Pages written | **10/10** |
| Planned / stale | 0 / 0 |
| `wiki check --strict` | **exit 0, 0 issues** |
| `path:line` citations | **1,598**, all resolvable |
| Total page bytes | ~200 KB |
| Semantic review | overview **pass, score 90** after fixing 6 reviewer findings; other pages still need semantic review |

The six overview review findings were real (Nix citation off by three lines,
duplicated `tsconfig.json:43`, incomplete component list, unsupported
`build:offline` causality, and two uncited/overstated inferences); they were
fixed and the page was re-reviewed to **pass**. This is the intended loop:
generate → mechanical check → independent reviewer → repair → re-review.
The final pi wiki should not be called semantically accepted until that
reviewer path passes all pages.

### Independent semantic audit of generated pages

An independent read-only audit sampled 23 `path:line` claims across the three
current written pages and checked the cited source lines:

| Result | Count |
|---|---:|
| Directly correct | 13 |
| Imprecise function-header/range reference | 4 |
| Wrong line (claim exists nearby) | 5 |
| False surrounding claim | 1 |

All **341** distinct `path:line` references in the three written pages resolve
to an existing file and an in-range line; mechanical citation resolution is
therefore 100%, but semantic line precision is not perfect. `overview.md` and
`testing.md` are strong and readable; `key-flows.md` is substantive but
relay-heavy and omits the core agent-turn flow. Completeness is **3 written /
3 stale / 2 missing** pages; the phone module plan lists 13 of 19 paths that no
longer exist after the concurrent phone asset move.

## A/B metrics (routing v2)

Main suite, 36 unique runs (3 reps/task/arm), total tokens:

| Task | Base median | Memory median | Δ | Success |
|---|---:|---:|---:|---:|
| Q1 scheduler | 223,852 | 153,311 | -31.5% | 3/3 + 3/3 |
| Q2 context-mode | 230,405 | 261,579 | +13.5% | 3/3 + 3/3 |
| Q3 memory-builtin | 144,665 | 116,371 | -19.6% | 3/3 + 3/3 |
| **Q&A all** | **210,419** | **147,132** | **-30.1%** | **9/9 + 9/9** |
| C1 cache bug | 82,143 | 78,324 | -4.6% | 3/3 + 3/3 |
| C2 config refactor | 60,703 | 78,536 | +29.4% (sampling noise) | 3/3 + 3/3 |
| C3 feature median | 41,569 | 41,366 | -0.5% | 3/3 + 3/3 |
| **Coding routing gate** | — | — | **0/9 injections, 0% prompt overhead** | **9/9 + 9/9** |

The C prompts are byte-identical between arms (96/118/93 chars); the total-token
spread is model sampling noise, not memory context.

Multi-session (plan session 1, implement session 2; 3 reps/arm):

| Metric | Base | Memory | Δ |
|---|---:|---:|---:|
| Total tokens | 129,708 | 70,327 | -45.8% |
| Tool calls | 31 | 27 | -12.9% |
| Success | 3/3 | 3/3 | — |

Policy structure (N=2 per arm; token medians noisy): auto injected **0/6**
times and used 96 median prompt chars; `always` injected **6/6** times and used
913 chars.

## Artifacts

- Raw A/B: `.ab-test/results/runs.jsonl`, `policy-runs.jsonl`, `multisession.json`
- Rendered report: `pir/docs/memory-builtin-ab-2026-11-09.md`
- Full test log: `.wiki-eval/pir-full-test.log`
- Review/fix record: `pir/docs/memory-builtin-review-2026-11-09.md`
