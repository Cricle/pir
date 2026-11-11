# `builtin:memory` — 3-agent cross-review (2026-11-09)

Three independent reviewers audited the embedded memory feature:

1. **API/contract review** — plugin lifecycle, tool surface, settings, host/SDK
   re-wiring, path resolution, feature gating, test coverage.
2. **A/B methodology review** — control/treatment isolation, raw JSONL
   reproducibility, provider/policy provenance, keyword gates, outlier and
   retry accounting.
3. **Accuracy/security review** — recall correctness, source grounding,
   import/trust laundering, concurrency, task-frame selection, cache
   invalidation, store maintenance.

## Disposition of findings

| # | Finding | Severity | Status / fix |
|---|---|---|---|
| 1 | Embedded import could mint `verified` trust, keep caller ids, write `frame`/`feedback` records, reserved `__cm_*` scopes, or supersede/merge away existing records | Critical | Fixed: `ops::sanitize_imported_memory` enforces `SourcePolicy`, rejects meta kinds/scopes, caps trust at `imported`, regenerates ids, strips supersede/source/merge metadata; `Cm::import_jsonl(_text)` both use it |
| 2 | Model `remember`/`append` could self-assign `verified` trust or create meta records | High | Fixed: tool writes reject `frame`/`feedback`/reserved scopes; trust is capped at `authored` (`inferred` may be requested) |
| 3 | `remember --replace` missed index records above 10k docs and superseded only one duplicate | High | Fixed: `find_subject_memories` scans the full index without the 10k cutoff and supersedes every active duplicate under the store lock |
| 4 | Auto-recall skipped the relevance floor whenever a frame existed; `always` had no floor | High | Fixed: `BriefOptions.min_score` is threaded into `select_brief`/`select_context_scored`; the floor applies to recalled memories in every policy, frames are exempt |
| 5 | `--verify-source`/stored source hash anchored against process cwd, not `SourcePolicy.root` | High | Fixed: `remember_with_at`/`append_input_at` pass the policy root into `memory_from_input_at`; library facade always supplies it |
| 6 | Imported `source` paths could escape the project in audit/snippet joins | High | Fixed: single `confined_source_path` rejects absolute/`..`, canonicalizes and requires `starts_with(canonical root)`; audit reports `unsafe_source` instead of reading outside |
| 7 | Config/brief caches never invalidated on cwd/trust change; auto-brief could cache a pre-write block | High | Fixed: cwd/trust changes call `reload`; brief cache is keyed by store `(len,mtime)` and a generation counter prevents in-flight writes from re-caching stale blocks |
| 8 | BM25 >10k-doc fast path scored only the rarest term's postings | Medium | Fixed: candidate union of all term postings, ranked by summed term frequency before the bounded BM25 pass |
| 9 | Hidden/superseded docs could consume top-N slots | Medium | Fixed: hidden doc ids are resolved before ranking and passed to `Store::bm25`; fetch over-fetches 8× |
| 10 | Consolidation/manual merge kept the highest-confidence record rather than the newest | Medium | Fixed: newest `created_at` is canonical; confidence breaks ties |
| 11 | Audit `--refresh` could resurrect a concurrently forgotten record | Medium | Fixed: re-check `active_memory_exists` under the store lock before the refresh append |
| 12 | In-place checkpoint could race an mmap reader | Medium | Fixed: `Store::open_with` retries once after a concurrent header/mmap length change |
| 13 | Automatic recall did not show trust; explicit recall used a stricter floor than auto | Medium | Fixed: `BriefMemory`/recall blocks include `trust`; explicit recall defaults to `imported` like auto |
| 14 | Explicit `done` or implicit `paused` frames could be injected | Medium | Fixed: `latest_frame` rejects `done` for explicit tasks and excludes `paused` from implicit current |
| 15 | `get` returned expired records | Medium | Fixed: `Cm::get`/`ops::get_memory` filter `valid_to` |
| 16 | Read-only/disk-cache `StoreOptions` were ignored by `search`/`brief`/`pack`/`frame list` | Medium | Fixed: `search_out_with`, `select_brief_with`, `pack_prompt_with`, `frame_*_value_with`; `Cm` passes its own options |
| 17 | `Cm::status` over-counted tombstoned docs | Low | Fixed: hidden doc ids are subtracted, tail memories added |
| 18 | `memory_snippet` could panic on a lowercase byte offset that is not a char boundary | Low | Fixed: boundary back-off |
| 19 | Tool CMX rendering always used `CM1F`, did not clean all fields | Low | Fixed: merged flag and tab/newline cleaning mirror the library line |
| 20 | A/B report pointed at stale phase-1 data, hid the provider switch, and never reported the policy experiment | Critical (report) | Fixed: report regenerated from one post-fix dataset with provider/model, binary hash, total/input/output/cache breakdown, attempts and explicit caveats; policy and multi-session tables included when run |
| 21 | Auto recall on plain short edits paid prompt/token overhead without reliable benefit | High | Fixed with routing v2: plain short edits get no injection (0/9 memory-context, 0% prompt chars); continuation short edits get frame-only context; multi-session stays at -45.8% tokens |

## Verification performed after the fixes

- `tools/cm-rs`: `cargo test` — all suites green (including new
  `import_rejects_meta_records_and_cannot_launder_trust`,
  `replace_supersedes_every_active_duplicate`,
  `concurrent_replace_leaves_one_active_record`).
- `tools/cm-rs`: `cargo check --no-default-features` — library-only build green.
- `pir`: `cargo check --no-default-features --features memory` and
  `cargo check --features full` — green.
- `pir`: `memory_builtin` integration tests — 17/17 green (including the wiki-plan action), including
  auto-recall policy, irrelevant-memory skip, auto-brief, session path,
  read-only, and the new trust/meta write test.
- `pir`: pinned `builtin_cli_audit`, `plugins_units`, `plugins_unified`,
  `plugin_events_w16`, `sdk` suites — green.
- Final post-fix A/B: see
  [memory-builtin-ab-2026-11-09.md](memory-builtin-ab-2026-11-09.md).

## Residual risks

- Q&A scoring is keyword coverage plus citation verification, not a semantic
  rubric; Q3 is partly self-referential because the store describes the memory
  plugin itself.
- The store's >10k-doc BM25 path is now a complete union within a 50k
  candidate bound; pathological corpora can still exceed that bound.
- `wiki generate`/`review` remain opt-in (`allowExternalCommands=false` by
  default) because they spawn external commands.
- `cm-memory 0.3.1` must be published before `pir` is packaged for crates.io;
  the workspace path dependency is for local development.
